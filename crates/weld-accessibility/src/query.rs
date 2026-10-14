//! Selection and work limits for one accessibility inspection.

use anyhow::{Result, ensure};

/// Filters are combined; depth zero includes only the accessible window itself.
#[derive(Clone, Debug)]
pub struct Query {
    pub max_nodes: u32,
    pub max_depth: u32,
    /// Case-insensitive substring of a node's name or application-supplied ID.
    pub query: Option<String>,
    /// Case-insensitive exact AT-SPI role, such as "button" or "text".
    pub role: Option<String>,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            max_nodes: 256,
            max_depth: 24,
            query: None,
            role: None,
        }
    }
}

impl Query {
    pub(crate) fn normalized(mut self) -> Result<Self> {
        ensure!(
            (1..=1024).contains(&self.max_nodes),
            "max_nodes must be between 1 and 1024"
        );
        ensure!(self.max_depth <= 24, "max_depth must be between 0 and 24");
        for text in [&mut self.query, &mut self.role].into_iter().flatten() {
            ensure!(
                text.len() <= 256,
                "accessibility filters must be at most 256 bytes"
            );
            *text = text.to_lowercase();
        }
        Ok(self)
    }

    pub(crate) fn matches(&self, role: &str, name: &str, accessible_id: Option<&str>) -> bool {
        self.role
            .as_ref()
            .is_none_or(|filter| role.to_lowercase() == *filter)
            && self.query.as_ref().is_none_or(|filter| {
                name.to_lowercase().contains(filter)
                    || accessible_id.is_some_and(|id| id.to_lowercase().contains(filter))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_id_search_combine_with_role_and_validate_work_limits() {
        let filter = Query {
            query: Some("SAVE".into()),
            role: Some("BUTTON".into()),
            ..Query::default()
        }
        .normalized()
        .expect("query");
        assert!(filter.matches("button", "Save as", None));
        assert!(filter.matches("button", "Write", Some("save-action")));
        assert!(!filter.matches("label", "Save as", None));
        assert!(!filter.matches("button", "Open", None));
        assert!(
            Query {
                max_nodes: 0,
                ..Query::default()
            }
            .normalized()
            .is_err()
        );
        assert!(
            Query {
                max_depth: 25,
                ..Query::default()
            }
            .normalized()
            .is_err()
        );
    }
}
