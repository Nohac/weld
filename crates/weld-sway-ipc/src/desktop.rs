//! Owned desktop geometry published atomically to local IPC readers.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Root,
    Output,
    Workspace,
    Con,
    FloatingCon,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    #[default]
    None,
    Output,
    Splith,
    Splitv,
    Tabbed,
    Stacked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct X11Properties {
    pub class: String,
    pub instance: String,
    pub title: String,
}

/// A Sway-shaped tree node. IDs remain stable for the represented object's lifetime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Node {
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: NodeType,
    pub name: Option<String>,
    pub rect: Rect,
    pub focused: bool,
    pub layout: Layout,
    pub nodes: Vec<Node>,
    pub floating_nodes: Vec<Node>,
    pub focus: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_properties: Option<X11Properties>,
    pub visible: bool,
    pub window_rect: Rect,
    pub fullscreen_mode: u8,
    pub sticky: bool,
}

impl Node {
    pub fn new(id: u64, kind: NodeType, rect: Rect) -> Self {
        Self {
            id,
            kind,
            name: None,
            rect,
            focused: false,
            layout: Layout::None,
            nodes: Vec::new(),
            floating_nodes: Vec::new(),
            focus: Vec::new(),
            pid: None,
            app_id: None,
            window_properties: None,
            visible: false,
            window_rect: Rect::default(),
            fullscreen_mode: 0,
            sticky: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Output {
    pub name: String,
    pub active: bool,
    pub focused: bool,
    pub primary: bool,
    pub rect: Rect,
    pub scale: f32,
    pub current_workspace: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DesktopSnapshot {
    pub tree: Node,
    pub outputs: Vec<Output>,
}

impl Default for DesktopSnapshot {
    fn default() -> Self {
        let mut tree = Node::new(0, NodeType::Root, Rect::default());
        tree.name = Some("root".into());
        Self {
            tree,
            outputs: Vec::new(),
        }
    }
}
