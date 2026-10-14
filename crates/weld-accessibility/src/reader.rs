//! Window association and bounded traversal on the accessibility bus.

use crate::{Bounds, Node, Query, Snapshot, Status};
use anyhow::{Context, Result, ensure};
use atspi::{
    AccessibilityConnection, CoordType, Interface, ObjectRefOwned, Role,
    proxy::{accessible::ObjectRefExt, action::ActionProxy, component::ComponentProxy},
    zbus::{Connection, fdo::DBusProxy, proxy::CacheProperties},
};
use std::{
    collections::{HashSet, VecDeque},
    future::Future,
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

const MAX_VISITED_NODES: usize = 1024;
const MAX_APPLICATIONS: i32 = 128;
const MAX_LABEL_BYTES: usize = 1024;

/// Inspect a process that the caller has associated with exactly one managed
/// native window. The callback revalidates its view permission during traversal.
pub async fn inspect(
    process_id: u32,
    query: Query,
    authorized: impl Fn() -> Result<()>,
) -> Result<Snapshot> {
    let query = query.normalized()?;
    authorized()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let result = timeout_at(deadline, read(process_id, &query, deadline, &authorized)).await;
    authorized()?;
    match result {
        Ok(result) => result,
        Err(_) => Ok(Snapshot::unavailable("accessibility provider timed out")),
    }
}

async fn read(
    process_id: u32,
    query: &Query,
    deadline: Instant,
    authorized: &impl Fn() -> Result<()>,
) -> Result<Snapshot> {
    let connection = match AccessibilityConnection::new().await {
        Ok(connection) => connection,
        Err(_) => {
            return Ok(Snapshot::unavailable(
                "session accessibility bus unavailable",
            ));
        }
    };
    let windows = process_windows(&connection, process_id, authorized).await?;
    let window = match select_window(windows) {
        Ok(Some(window)) => window,
        Ok(None) => {
            return Ok(Snapshot::unavailable(
                "application has no registered accessible window",
            ));
        }
        Err(error) => return Ok(Snapshot::ambiguous(error.to_string())),
    };
    let snapshot = traverse(
        connection.connection(),
        window.clone(),
        query,
        deadline - Duration::from_millis(250),
        authorized,
    )
    .await?;
    let latest = process_windows(&connection, process_id, authorized).await?;
    ensure!(
        latest == [window],
        "accessible window association changed during inspection"
    );
    Ok(snapshot)
}

async fn process_windows(
    connection: &AccessibilityConnection,
    process_id: u32,
    authorized: &impl Fn() -> Result<()>,
) -> Result<Vec<ObjectRefOwned>> {
    let root = connection.root_accessible_on_registry().await?;
    let count = root.child_count().await?;
    ensure!(
        (0..=MAX_APPLICATIONS).contains(&count),
        "accessibility application inventory exceeds limit"
    );
    let bus = connection.connection();
    let dbus = DBusProxy::new(bus).await?;
    let mut windows = Vec::new();
    for index in 0..count {
        authorized()?;
        let application = root.get_child_at_index(index).await?;
        let Some(name) = application.name() else {
            continue;
        };
        if dbus
            .get_connection_unix_process_id(name.clone().into())
            .await
            .ok()
            != Some(process_id)
        {
            continue;
        }
        let application_proxy = application.as_accessible_proxy(bus).await?;
        let children = application_proxy.child_count().await?;
        ensure!(
            (0..=128).contains(&children),
            "accessibility window inventory exceeds limit"
        );
        for index in 0..children {
            let window = application_proxy.get_child_at_index(index).await?;
            if window.is_null() {
                continue;
            }
            ensure!(
                window.name() == application.name(),
                "cross-process accessibility window association requires verification"
            );
            let proxy = window.as_accessible_proxy(bus).await?;
            ensure!(
                matches!(
                    proxy.get_role().await?,
                    Role::Frame | Role::Dialog | Role::Window
                ),
                "application exposes an unsupported top-level accessibility role"
            );
            windows.push(window);
        }
    }
    Ok(windows)
}

fn select_window(mut windows: Vec<ObjectRefOwned>) -> Result<Option<ObjectRefOwned>> {
    ensure!(
        windows.len() <= 1,
        "multiple accessible windows belong to this process; exact window association is required"
    );
    Ok(windows.pop())
}

struct Reader<'a, F> {
    bus: &'a Connection,
    deadline: Instant,
    authorized: &'a F,
}

impl<F: Fn() -> Result<()>> Reader<'_, F> {
    async fn optional<T>(
        &self,
        operation: impl Future<Output = atspi::zbus::Result<T>>,
    ) -> Result<Option<T>> {
        (self.authorized)()?;
        Ok(timeout_at(self.deadline, operation)
            .await
            .ok()
            .and_then(Result::ok))
    }

    async fn node(
        &self,
        object: &ObjectRefOwned,
        id: u32,
        parent: Option<u32>,
        depth: u32,
        query: &Query,
    ) -> Result<Option<Node>> {
        let proxy = object.as_accessible_proxy(self.bus).await?;
        let role = proxy.get_role().await?;
        let name = self.optional(proxy.name()).await?;
        let accessible_id = self.optional(proxy.accessible_id()).await?;
        if !query.matches(
            role.name(),
            name.as_deref().unwrap_or_default(),
            accessible_id.as_deref(),
        ) {
            return Ok(None);
        }
        let states = self.optional(proxy.get_state()).await?;
        let interfaces = self.optional(proxy.get_interfaces()).await?;
        let mut incomplete = name.is_none()
            || states.is_none()
            || interfaces.is_none()
            || name
                .as_ref()
                .is_some_and(|name| name.len() > MAX_LABEL_BYTES)
            || accessible_id
                .as_ref()
                .is_some_and(|id| id.len() > MAX_LABEL_BYTES);
        let mut bounds = None;
        let mut actions = Vec::new();
        if let Some(interfaces) = interfaces {
            if interfaces.contains(Interface::Component) {
                let component = ComponentProxy::builder(self.bus)
                    .destination(object.name().context("missing bus owner")?.clone())?
                    .path(object.path())?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await?;
                if let Some((x, y, width, height)) = self
                    .optional(component.get_extents(CoordType::Window))
                    .await?
                {
                    if width >= 0 && height >= 0 && x != i32::MIN && y != i32::MIN {
                        bounds = Some(Bounds {
                            x,
                            y,
                            width,
                            height,
                        });
                    } else {
                        incomplete = true;
                    }
                } else {
                    incomplete = true;
                }
            }
            if interfaces.contains(Interface::Action) {
                let action = ActionProxy::builder(self.bus)
                    .destination(object.name().context("missing bus owner")?.clone())?
                    .path(object.path())?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await?;
                if let Some(count) = self.optional(action.n_actions()).await? {
                    incomplete |= !(0..=16).contains(&count);
                    for index in 0..count.clamp(0, 16) {
                        if let Some(name) = self.optional(action.get_name(index)).await? {
                            incomplete |= name.len() > MAX_LABEL_BYTES;
                            actions.push(label(name));
                        } else {
                            incomplete = true;
                        }
                    }
                } else {
                    incomplete = true;
                }
            }
        }
        Ok(Some(Node {
            id,
            parent,
            depth,
            role: role.to_string(),
            name: label(name.unwrap_or_default()),
            accessible_id: accessible_id.filter(|id| !id.is_empty()).map(label),
            states: states
                .map(|states| {
                    states
                        .iter()
                        .map(|state| state.to_static_str().to_owned())
                        .collect()
                })
                .unwrap_or_default(),
            bounds,
            actions,
            incomplete,
        }))
    }
}

async fn traverse(
    bus: &Connection,
    root: ObjectRefOwned,
    query: &Query,
    deadline: Instant,
    authorized: &impl Fn() -> Result<()>,
) -> Result<Snapshot> {
    let owner = root.name().cloned();
    let reader = Reader {
        bus,
        deadline,
        authorized,
    };
    let mut snapshot = Snapshot {
        status: Status::Ready,
        reason: None,
        nodes: Vec::new(),
        truncated: false,
        visited_nodes: 0,
    };
    let mut pending = VecDeque::from([(root, None, 0)]);
    let mut visited = HashSet::new();
    while let Some((object, parent, depth)) = pending.pop_front() {
        authorized()?;
        if Instant::now() >= deadline
            || snapshot.nodes.len() >= query.max_nodes as usize
            || visited.len() >= MAX_VISITED_NODES
        {
            snapshot.truncated = true;
            break;
        }
        if object.is_null() || !visited.insert(object.clone()) {
            continue;
        }
        if object.name() != owner.as_ref() {
            snapshot.truncated = true;
            continue;
        }
        let id = snapshot.visited_nodes;
        snapshot.visited_nodes += 1;
        match timeout_at(deadline, reader.node(&object, id, parent, depth, query)).await {
            Ok(Ok(Some(node))) => snapshot.nodes.push(node),
            Ok(Ok(None)) => {}
            Ok(Err(_)) => {
                authorized()?;
                snapshot.truncated = true;
                continue;
            }
            Err(_) => {
                snapshot.truncated = true;
                break;
            }
        }
        let proxy = object.as_accessible_proxy(bus).await?;
        let count = reader.optional(proxy.child_count()).await?;
        let Some(count) = count.filter(|count| *count >= 0) else {
            snapshot.truncated = true;
            continue;
        };
        let remaining = MAX_VISITED_NODES.saturating_sub(visited.len() + pending.len());
        let take = if depth >= query.max_depth {
            0
        } else {
            (count as usize).min(remaining)
        };
        snapshot.truncated |= take < count as usize;
        for index in 0..take {
            if let Some(child) = reader
                .optional(proxy.get_child_at_index(i32::try_from(index)?))
                .await?
            {
                pending.push_back((child, Some(id), depth + 1));
            } else {
                snapshot.truncated = true;
            }
        }
    }
    authorized()?;
    Ok(snapshot)
}

fn label(mut text: String) -> String {
    let mut end = text.len().min(MAX_LABEL_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use atspi::{
        ObjectRef,
        zbus::{names::UniqueName, zvariant::ObjectPath},
    };

    #[test]
    fn ambiguous_windows_are_never_resolved_by_name_or_order() {
        let one = ObjectRef::new_owned(
            UniqueName::try_from(":1.2").expect("name"),
            ObjectPath::try_from("/window/one").expect("path"),
        );
        let two = ObjectRef::new_owned(
            UniqueName::try_from(":1.2").expect("name"),
            ObjectPath::try_from("/window/two").expect("path"),
        );
        assert!(select_window(vec![one.clone(), two]).is_err());
        assert_eq!(select_window(vec![one.clone()]).expect("unique"), Some(one));
        assert_eq!(label("é".repeat(600)).len(), 1024);
    }
}
