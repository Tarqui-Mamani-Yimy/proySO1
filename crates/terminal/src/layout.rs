//! Pure pane geometry, split-tree and tab state for the terminal UI.

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PaneId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TabId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// Divide the available width into left and right panes.
    Horizontal,
    /// Divide the available height into top and bottom panes.
    Vertical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn right(self) -> u32 {
        self.x.saturating_add(self.width)
    }

    fn bottom(self) -> u32 {
        self.y.saturating_add(self.height)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellSize {
    pub width: u32,
    pub height: u32,
}

impl CellSize {
    pub const fn new(width: u32, height: u32) -> Self {
        Self {
            width: if width == 0 { 1 } else { width },
            height: if height == 0 { 1 } else { height },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaneLayout {
    pub pane: PaneId,
    pub rect: Rect,
    pub cols: u32,
    pub rows: u32,
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitError {
    PaneNotFound,
    MinimumSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseResult {
    Closed { focus: PaneId },
    LastPane,
    PaneNotFound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabCloseResult {
    Closed(TabId),
    LastTab,
}

#[derive(Clone, Debug)]
enum Node {
    Leaf(PaneId),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Clone, Copy, Debug)]
struct MinPixels {
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug)]
struct LayoutConfig {
    window: Rect,
    cell: CellSize,
    divider: u32,
    min_cols: u32,
    min_rows: u32,
}

#[derive(Clone, Debug)]
pub struct LayoutTree {
    root: Node,
    next_pane: u64,
    config: LayoutConfig,
}

impl LayoutTree {
    pub fn new(
        window: Rect,
        cell: CellSize,
        divider_width: u32,
        min_cols: u32,
        min_rows: u32,
    ) -> Self {
        Self {
            root: Node::Leaf(PaneId(1)),
            next_pane: 2,
            config: LayoutConfig {
                window,
                cell: CellSize::new(cell.width, cell.height),
                divider: divider_width,
                min_cols: min_cols.max(1),
                min_rows: min_rows.max(1),
            },
        }
    }

    pub fn root_pane(&self) -> PaneId {
        first_pane(&self.root)
    }

    pub fn panes(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        collect_panes(&self.root, &mut panes);
        panes
    }

    pub fn resize_window(&mut self, rect: Rect) {
        self.config.window = rect;
    }

    pub fn split(&mut self, focused: PaneId, direction: Direction) -> Result<PaneId, SplitError> {
        let current = self
            .layout(focused)
            .into_iter()
            .find(|pane| pane.pane == focused)
            .ok_or(SplitError::PaneNotFound)?;
        if current.hidden || !self.can_split(current.rect, direction) {
            return Err(SplitError::MinimumSize);
        }

        let created = PaneId(self.next_pane);
        if !replace_leaf(&mut self.root, focused, direction, created) {
            return Err(SplitError::PaneNotFound);
        }
        self.next_pane = self.next_pane.saturating_add(1);
        Ok(created)
    }

    fn can_split(&self, rect: Rect, direction: Direction) -> bool {
        let two_min = match direction {
            Direction::Horizontal => MinPixels {
                width: self
                    .config
                    .min_cols
                    .saturating_mul(self.config.cell.width)
                    .saturating_mul(2)
                    .saturating_add(self.config.divider),
                height: self.config.min_rows.saturating_mul(self.config.cell.height),
            },
            Direction::Vertical => MinPixels {
                width: self.config.min_cols.saturating_mul(self.config.cell.width),
                height: self
                    .config
                    .min_rows
                    .saturating_mul(self.config.cell.height)
                    .saturating_mul(2)
                    .saturating_add(self.config.divider),
            },
        };
        rect.width >= two_min.width && rect.height >= two_min.height
    }

    /// Layout includes hidden leaves. If a split no longer fits, retain the
    /// focused branch when it meets minimum size; otherwise use the first
    /// branch that fits. If none fits, every leaf is marked hidden.
    pub fn layout(&self, focused: PaneId) -> Vec<PaneLayout> {
        let mut result = Vec::new();
        self.layout_node(&self.root, self.config.window, focused, &mut result);
        result
    }

    fn layout_node(&self, node: &Node, rect: Rect, focused: PaneId, result: &mut Vec<PaneLayout>) {
        if !self.fits(node, rect) {
            if let Node::Split { first, second, .. } = node {
                let first_has_focus = contains(first, focused);
                let second_has_focus = contains(second, focused);
                if first_has_focus && self.fits(first, rect) {
                    self.layout_node(first, rect, focused, result);
                    self.hide_node(second, result);
                    return;
                }
                if second_has_focus && self.fits(second, rect) {
                    self.hide_node(first, result);
                    self.layout_node(second, rect, focused, result);
                    return;
                }
                if self.fits(first, rect) {
                    self.layout_node(first, rect, focused, result);
                    self.hide_node(second, result);
                    return;
                }
                if self.fits(second, rect) {
                    self.hide_node(first, result);
                    self.layout_node(second, rect, focused, result);
                    return;
                }
            }
            self.hide_node(node, result);
            return;
        }

        match node {
            Node::Leaf(pane) => result.push(self.visible_layout(*pane, rect)),
            Node::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (first_rect, second_rect) =
                    self.child_rects(*direction, *ratio, first, second, rect);
                self.layout_node(first, first_rect, focused, result);
                self.layout_node(second, second_rect, focused, result);
            }
        }
    }

    fn hide_node(&self, node: &Node, result: &mut Vec<PaneLayout>) {
        match node {
            Node::Leaf(pane) => result.push(PaneLayout {
                pane: *pane,
                rect: Rect::new(0, 0, 0, 0),
                cols: 0,
                rows: 0,
                hidden: true,
            }),
            Node::Split { first, second, .. } => {
                self.hide_node(first, result);
                self.hide_node(second, result);
            }
        }
    }

    fn visible_layout(&self, pane: PaneId, rect: Rect) -> PaneLayout {
        let cols = rect.width / self.config.cell.width;
        let rows = rect.height / self.config.cell.height;
        if cols < self.config.min_cols || rows < self.config.min_rows {
            return PaneLayout {
                pane,
                rect: Rect::new(rect.x, rect.y, 0, 0),
                cols: 0,
                rows: 0,
                hidden: true,
            };
        }
        PaneLayout {
            pane,
            rect: Rect::new(
                rect.x,
                rect.y,
                cols.saturating_mul(self.config.cell.width),
                rows.saturating_mul(self.config.cell.height),
            ),
            cols,
            rows,
            hidden: false,
        }
    }

    fn fits(&self, node: &Node, rect: Rect) -> bool {
        let min = self.minimum_pixels(node);
        rect.width >= min.width && rect.height >= min.height
    }

    fn minimum_pixels(&self, node: &Node) -> MinPixels {
        match node {
            Node::Leaf(_) => MinPixels {
                width: self.config.min_cols.saturating_mul(self.config.cell.width),
                height: self.config.min_rows.saturating_mul(self.config.cell.height),
            },
            Node::Split {
                direction,
                first,
                second,
                ..
            } => {
                let first = self.minimum_pixels(first);
                let second = self.minimum_pixels(second);
                match direction {
                    Direction::Horizontal => MinPixels {
                        width: first
                            .width
                            .saturating_add(self.config.divider)
                            .saturating_add(second.width),
                        height: first.height.max(second.height),
                    },
                    Direction::Vertical => MinPixels {
                        width: first.width.max(second.width),
                        height: first
                            .height
                            .saturating_add(self.config.divider)
                            .saturating_add(second.height),
                    },
                }
            }
        }
    }

    fn child_rects(
        &self,
        direction: Direction,
        ratio: f32,
        first: &Node,
        second: &Node,
        rect: Rect,
    ) -> (Rect, Rect) {
        let (extent, cell, origin, cross_origin, cross_extent) = match direction {
            Direction::Horizontal => (
                rect.width,
                self.config.cell.width,
                rect.x,
                rect.y,
                rect.height,
            ),
            Direction::Vertical => (
                rect.height,
                self.config.cell.height,
                rect.y,
                rect.x,
                rect.width,
            ),
        };
        let available_cells = extent.saturating_sub(self.config.divider) / cell;
        let first_min = self.minimum_pixels(first);
        let second_min = self.minimum_pixels(second);
        let (first_pixels, second_pixels) = match direction {
            Direction::Horizontal => (first_min.width, second_min.width),
            Direction::Vertical => (first_min.height, second_min.height),
        };
        let min_first = div_ceil(first_pixels, cell).min(available_cells);
        let min_second = div_ceil(second_pixels, cell).min(available_cells);
        let wanted_first = ((available_cells as f64) * (ratio as f64)).floor() as u32;
        let max_first = available_cells.saturating_sub(min_second);
        let first_cells = wanted_first.clamp(min_first, max_first.max(min_first));
        let second_cells = available_cells.saturating_sub(first_cells);
        let first_span = first_cells.saturating_mul(cell);
        let second_span = second_cells.saturating_mul(cell);
        let second_origin = origin
            .saturating_add(first_span)
            .saturating_add(self.config.divider);

        match direction {
            Direction::Horizontal => (
                Rect::new(origin, cross_origin, first_span, cross_extent),
                Rect::new(second_origin, cross_origin, second_span, cross_extent),
            ),
            Direction::Vertical => (
                Rect::new(cross_origin, origin, cross_extent, first_span),
                Rect::new(cross_origin, second_origin, cross_extent, second_span),
            ),
        }
    }

    pub fn hit_test(&self, x: u32, y: u32) -> Option<PaneId> {
        self.hit_test_with_focus(x, y, self.root_pane())
    }

    fn hit_test_with_focus(&self, x: u32, y: u32, focused: PaneId) -> Option<PaneId> {
        self.layout(focused)
            .into_iter()
            .find(|pane| {
                !pane.hidden
                    && x >= pane.rect.x
                    && x < pane.rect.right()
                    && y >= pane.rect.y
                    && y < pane.rect.bottom()
            })
            .map(|pane| pane.pane)
    }

    pub fn focus_target(&self, focused: PaneId, direction: FocusDirection) -> Option<PaneId> {
        let panes = self.layout(focused);
        let source = panes
            .iter()
            .find(|pane| pane.pane == focused && !pane.hidden)?;
        panes
            .iter()
            .filter(|candidate| candidate.pane != focused && !candidate.hidden)
            .filter_map(|candidate| {
                focus_score(source.rect, candidate.rect, candidate.pane, direction)
                    .map(|score| (candidate.pane, score))
            })
            .min_by(|left, right| left.1.cmp(&right.1))
            .map(|(pane, _)| pane)
    }

    pub fn close(&mut self, pane: PaneId) -> CloseResult {
        if !contains(&self.root, pane) {
            return CloseResult::PaneNotFound;
        }
        if self.panes().len() == 1 {
            return CloseResult::LastPane;
        }
        let old_root = self.root.clone();
        match remove_leaf(old_root, pane) {
            Some(root) => {
                self.root = root;
                CloseResult::Closed {
                    focus: first_pane(&self.root),
                }
            }
            None => CloseResult::LastPane,
        }
    }

    pub fn ratio_for(&self, pane: PaneId) -> Option<f32> {
        immediate_parent_ratio(&self.root, pane)
    }

    /// Adjust the focused pane's nearest parent ratio by 5% per step. The
    /// ratio is clamped so both child subtrees retain their minimum cell size.
    pub fn adjust_split(&mut self, pane: PaneId, steps: i32) -> bool {
        let Some((direction, ratio, rect, first, second)) =
            find_parent_layout(&self.root, pane, self.config.window, self)
        else {
            return false;
        };
        let (extent, cell) = match direction {
            Direction::Horizontal => (rect.width, self.config.cell.width),
            Direction::Vertical => (rect.height, self.config.cell.height),
        };
        let cells = extent.saturating_sub(self.config.divider) / cell;
        let first_min_px = match direction {
            Direction::Horizontal => self.minimum_pixels(first).width,
            Direction::Vertical => self.minimum_pixels(first).height,
        };
        let second_min_px = match direction {
            Direction::Horizontal => self.minimum_pixels(second).width,
            Direction::Vertical => self.minimum_pixels(second).height,
        };
        let min_first = div_ceil(first_min_px, cell).min(cells);
        let min_second = div_ceil(second_min_px, cell).min(cells);
        if cells < min_first.saturating_add(min_second) || cells == 0 {
            return false;
        }
        let min_ratio = min_first as f32 / cells as f32;
        let max_ratio = (cells - min_second) as f32 / cells as f32;
        let next = (ratio + steps as f32 * 0.05).clamp(min_ratio, max_ratio);
        set_immediate_parent_ratio(&mut self.root, pane, next)
    }
}

fn div_ceil(value: u32, divisor: u32) -> u32 {
    value / divisor + u32::from(!value.is_multiple_of(divisor))
}

fn contains(node: &Node, pane: PaneId) -> bool {
    match node {
        Node::Leaf(id) => *id == pane,
        Node::Split { first, second, .. } => contains(first, pane) || contains(second, pane),
    }
}

fn first_pane(node: &Node) -> PaneId {
    match node {
        Node::Leaf(pane) => *pane,
        Node::Split { first, .. } => first_pane(first),
    }
}

fn collect_panes(node: &Node, panes: &mut Vec<PaneId>) {
    match node {
        Node::Leaf(pane) => panes.push(*pane),
        Node::Split { first, second, .. } => {
            collect_panes(first, panes);
            collect_panes(second, panes);
        }
    }
}

fn replace_leaf(node: &mut Node, pane: PaneId, direction: Direction, new: PaneId) -> bool {
    match node {
        Node::Leaf(old) if *old == pane => {
            *node = Node::Split {
                direction,
                ratio: 0.5,
                first: Box::new(Node::Leaf(pane)),
                second: Box::new(Node::Leaf(new)),
            };
            true
        }
        Node::Leaf(_) => false,
        Node::Split { first, second, .. } => {
            replace_leaf(first, pane, direction, new) || replace_leaf(second, pane, direction, new)
        }
    }
}

fn remove_leaf(node: Node, pane: PaneId) -> Option<Node> {
    match node {
        Node::Leaf(id) => (id != pane).then_some(Node::Leaf(id)),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first = remove_leaf(*first, pane);
            let second = remove_leaf(*second, pane);
            match (first, second) {
                (Some(first), Some(second)) => Some(Node::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            }
        }
    }
}

fn immediate_parent_ratio(node: &Node, pane: PaneId) -> Option<f32> {
    match node {
        Node::Leaf(_) => None,
        Node::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if matches!(first.as_ref(), Node::Leaf(id) if *id == pane)
                || matches!(second.as_ref(), Node::Leaf(id) if *id == pane)
            {
                Some(*ratio)
            } else if contains(first, pane) {
                immediate_parent_ratio(first, pane).or(Some(*ratio))
            } else if contains(second, pane) {
                immediate_parent_ratio(second, pane).or(Some(*ratio))
            } else {
                None
            }
        }
    }
}

fn set_immediate_parent_ratio(node: &mut Node, pane: PaneId, value: f32) -> bool {
    match node {
        Node::Leaf(_) => false,
        Node::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if matches!(first.as_ref(), Node::Leaf(id) if *id == pane)
                || matches!(second.as_ref(), Node::Leaf(id) if *id == pane)
            {
                *ratio = value;
                true
            } else if contains(first, pane) {
                set_immediate_parent_ratio(first, pane, value)
            } else if contains(second, pane) {
                set_immediate_parent_ratio(second, pane, value)
            } else {
                false
            }
        }
    }
}

fn find_parent_layout<'a>(
    node: &'a Node,
    pane: PaneId,
    rect: Rect,
    tree: &LayoutTree,
) -> Option<(Direction, f32, Rect, &'a Node, &'a Node)> {
    let Node::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    else {
        return None;
    };
    let (first_rect, second_rect) = tree.child_rects(*direction, *ratio, first, second, rect);
    if matches!(first.as_ref(), Node::Leaf(id) if *id == pane)
        || matches!(second.as_ref(), Node::Leaf(id) if *id == pane)
    {
        return Some((*direction, *ratio, rect, first, second));
    }
    if contains(first, pane) {
        find_parent_layout(first, pane, first_rect, tree)
            .or(Some((*direction, *ratio, rect, first, second)))
    } else if contains(second, pane) {
        find_parent_layout(second, pane, second_rect, tree)
            .or(Some((*direction, *ratio, rect, first, second)))
    } else {
        None
    }
}

/// Candidate ranking: nearest edge first, then greatest perpendicular
/// overlap, then nearest perpendicular center, then top/left position and ID.
fn focus_score(
    source: Rect,
    candidate: Rect,
    pane: PaneId,
    direction: FocusDirection,
) -> Option<(u32, std::cmp::Reverse<u32>, u32, u32, u32, PaneId)> {
    let (gap, overlap, center_delta, position, in_direction) = match direction {
        FocusDirection::Right => (
            candidate.x.saturating_sub(source.right()),
            overlap(source.y, source.bottom(), candidate.y, candidate.bottom()),
            center_delta(source.y, source.height, candidate.y, candidate.height),
            (candidate.y, candidate.x),
            candidate.x >= source.right(),
        ),
        FocusDirection::Left => (
            source.x.saturating_sub(candidate.right()),
            overlap(source.y, source.bottom(), candidate.y, candidate.bottom()),
            center_delta(source.y, source.height, candidate.y, candidate.height),
            (candidate.y, candidate.x),
            candidate.right() <= source.x,
        ),
        FocusDirection::Down => (
            candidate.y.saturating_sub(source.bottom()),
            overlap(source.x, source.right(), candidate.x, candidate.right()),
            center_delta(source.x, source.width, candidate.x, candidate.width),
            (candidate.y, candidate.x),
            candidate.y >= source.bottom(),
        ),
        FocusDirection::Up => (
            source.y.saturating_sub(candidate.bottom()),
            overlap(source.x, source.right(), candidate.x, candidate.right()),
            center_delta(source.x, source.width, candidate.x, candidate.width),
            (candidate.y, candidate.x),
            candidate.bottom() <= source.y,
        ),
    };
    in_direction.then_some((
        gap,
        std::cmp::Reverse(overlap),
        center_delta,
        position.0,
        position.1,
        pane,
    ))
}

fn overlap(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> u32 {
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

fn center_delta(a_start: u32, a_extent: u32, b_start: u32, b_extent: u32) -> u32 {
    let a = u64::from(a_start) * 2 + u64::from(a_extent);
    let b = u64::from(b_start) * 2 + u64::from(b_extent);
    a.abs_diff(b).min(u64::from(u32::MAX)) as u32
}

#[derive(Clone, Debug)]
pub struct Tab {
    id: TabId,
    tree: LayoutTree,
    active_pane: PaneId,
}

impl Tab {
    fn new(id: TabId, tree: LayoutTree) -> Self {
        let active_pane = tree.root_pane();
        Self {
            id,
            tree,
            active_pane,
        }
    }

    pub fn id(&self) -> TabId {
        self.id
    }

    pub fn active_pane(&self) -> PaneId {
        self.active_pane
    }

    pub fn layout_tree(&self) -> &LayoutTree {
        &self.tree
    }

    pub fn layout_tree_mut(&mut self) -> &mut LayoutTree {
        &mut self.tree
    }

    pub fn split_focused(&mut self, direction: Direction) -> Result<PaneId, SplitError> {
        let pane = self.tree.split(self.active_pane, direction)?;
        self.active_pane = pane;
        Ok(pane)
    }

    pub fn close_pane(&mut self, pane: PaneId) -> CloseResult {
        let result = self.tree.close(pane);
        if let CloseResult::Closed { focus } = result
            && self.active_pane == pane
        {
            self.active_pane = focus;
        }
        result
    }

    pub fn move_focus(&mut self, direction: FocusDirection) -> Option<PaneId> {
        let target = self.tree.focus_target(self.active_pane, direction)?;
        self.active_pane = target;
        Some(target)
    }

    pub fn focus_by_click(&mut self, x: u32, y: u32) -> Option<PaneId> {
        let target = self.tree.hit_test_with_focus(x, y, self.active_pane)?;
        self.active_pane = target;
        Some(target)
    }

    pub fn layout(&self) -> Vec<PaneLayout> {
        self.tree.layout(self.active_pane)
    }
}

#[derive(Clone, Debug)]
pub struct TabSet {
    tabs: Vec<Tab>,
    active_index: usize,
    next_tab: u64,
    template: LayoutConfig,
}

impl TabSet {
    pub fn new(
        window: Rect,
        cell: CellSize,
        divider_width: u32,
        min_cols: u32,
        min_rows: u32,
    ) -> Self {
        let template = LayoutConfig {
            window,
            cell: CellSize::new(cell.width, cell.height),
            divider: divider_width,
            min_cols: min_cols.max(1),
            min_rows: min_rows.max(1),
        };
        let tree = LayoutTree::new(window, cell, divider_width, min_cols, min_rows);
        Self {
            tabs: vec![Tab::new(TabId(1), tree)],
            active_index: 0,
            next_tab: 2,
            template,
        }
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn tab_ids(&self) -> Vec<TabId> {
        self.tabs.iter().map(Tab::id).collect()
    }

    pub fn active_index(&self) -> Option<usize> {
        (!self.tabs.is_empty()).then_some(self.active_index)
    }

    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active_index)
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_index)
    }

    pub fn add_tab(&mut self) -> TabId {
        let id = TabId(self.next_tab);
        self.next_tab = self.next_tab.saturating_add(1);
        let config = self.template;
        let tree = LayoutTree::new(
            config.window,
            config.cell,
            config.divider,
            config.min_cols,
            config.min_rows,
        );
        self.tabs.push(Tab::new(id, tree));
        self.active_index = self.tabs.len() - 1;
        id
    }

    pub fn close_active(&mut self) -> TabCloseResult {
        if self.tabs.len() <= 1 {
            return TabCloseResult::LastTab;
        }
        let removed = self.tabs.remove(self.active_index);
        self.active_index = self.active_index.min(self.tabs.len() - 1);
        TabCloseResult::Closed(removed.id)
    }

    pub fn next(&mut self) {
        if !self.tabs.is_empty() {
            self.active_index = (self.active_index + 1) % self.tabs.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.tabs.is_empty() {
            self.active_index = (self.active_index + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return false;
        }
        self.active_index = index;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> LayoutTree {
        LayoutTree::new(Rect::new(0, 0, 1000, 600), CellSize::new(10, 10), 10, 5, 3)
    }

    fn visible(layout: &[PaneLayout], pane: PaneId) -> &PaneLayout {
        layout.iter().find(|entry| entry.pane == pane).unwrap()
    }

    #[test]
    fn nested_horizontal_and_vertical_splits_snap_to_cells() {
        let mut tree = tree();
        let first = tree.root_pane();
        let right = tree.split(first, Direction::Horizontal).unwrap();
        let below = tree.split(first, Direction::Vertical).unwrap();

        let layout = tree.layout(first);
        assert_eq!(visible(&layout, first).rect, Rect::new(0, 0, 490, 290));
        assert_eq!(visible(&layout, below).rect, Rect::new(0, 300, 490, 300));
        assert_eq!(visible(&layout, right).rect, Rect::new(500, 0, 500, 600));
        assert!(layout.iter().all(|pane| !pane.hidden));
        assert!(
            layout
                .iter()
                .all(|pane| pane.rect.width % 10 == 0 && pane.rect.height % 10 == 0)
        );
    }

    #[test]
    fn split_refuses_to_break_the_minimum_and_shrunk_window_hides_inactive_panes() {
        let mut tree = LayoutTree::new(Rect::new(0, 0, 300, 100), CellSize::new(10, 10), 10, 4, 2);
        let first = tree.root_pane();
        let second = tree.split(first, Direction::Horizontal).unwrap();
        tree.resize_window(Rect::new(0, 0, 70, 40));
        let shrunk = tree.layout(second);
        assert!(!visible(&shrunk, second).hidden);
        assert_eq!(
            (visible(&shrunk, second).cols, visible(&shrunk, second).rows),
            (7, 4)
        );
        assert!(visible(&shrunk, first).hidden);

        tree.resize_window(Rect::new(0, 0, 20, 10));
        assert!(tree.layout(second).iter().all(|pane| pane.hidden));
        assert!(matches!(
            tree.split(second, Direction::Vertical),
            Err(SplitError::MinimumSize)
        ));

        let mut tiny = LayoutTree::new(Rect::new(0, 0, 80, 40), CellSize::new(10, 10), 10, 4, 2);
        let only = tiny.root_pane();
        assert!(matches!(
            tiny.split(only, Direction::Horizontal),
            Err(SplitError::MinimumSize)
        ));
    }

    #[test]
    fn divider_is_not_hit_tested_as_a_pane() {
        let mut tree = LayoutTree::new(Rect::new(0, 0, 300, 100), CellSize::new(10, 10), 10, 4, 2);
        let first = tree.root_pane();
        let second = tree.split(first, Direction::Horizontal).unwrap();
        assert_eq!(tree.hit_test(20, 20), Some(first));
        assert_eq!(tree.hit_test(145, 20), None);
        assert_eq!(tree.hit_test(250, 20), Some(second));
        assert_eq!(tree.hit_test(20, 101), None);
    }

    #[test]
    fn focus_movement_in_an_l_layout_prefers_overlap_then_position() {
        let mut tree = tree();
        let top_left = tree.root_pane();
        let right = tree.split(top_left, Direction::Horizontal).unwrap();
        let bottom_left = tree.split(top_left, Direction::Vertical).unwrap();
        assert_eq!(
            tree.focus_target(bottom_left, FocusDirection::Right),
            Some(right)
        );
        assert_eq!(
            tree.focus_target(right, FocusDirection::Left),
            Some(bottom_left)
        );
        assert_eq!(
            tree.focus_target(top_left, FocusDirection::Down),
            Some(bottom_left)
        );
        assert_eq!(tree.focus_target(top_left, FocusDirection::Up), None);
    }

    #[test]
    fn closing_a_pane_collapses_its_parent_and_last_pane_is_explicit() {
        let mut tree = tree();
        let first = tree.root_pane();
        let second = tree.split(first, Direction::Horizontal).unwrap();
        let third = tree.split(second, Direction::Vertical).unwrap();
        assert_eq!(tree.close(second), CloseResult::Closed { focus: first });
        assert_eq!(tree.panes(), vec![first, third]);
        assert_eq!(tree.close(first), CloseResult::Closed { focus: third });
        assert_eq!(tree.close(third), CloseResult::LastPane);
        assert_eq!(tree.panes(), vec![third]);
    }

    #[test]
    fn split_ratio_adjustment_clamps_to_minimum_cell_counts() {
        let mut tree = LayoutTree::new(Rect::new(0, 0, 300, 100), CellSize::new(10, 10), 10, 4, 2);
        let first = tree.root_pane();
        let second = tree.split(first, Direction::Horizontal).unwrap();
        assert!(tree.adjust_split(first, 100));
        assert!(tree.ratio_for(first).unwrap() < 1.0);
        let layout = tree.layout(first);
        assert!(visible(&layout, first).cols >= 4);
        assert!(visible(&layout, second).cols >= 4);
        assert!(tree.adjust_split(second, -100));
        assert!(tree.ratio_for(second).unwrap() > 0.0);
        assert!(tree.layout(second).iter().all(|pane| pane.cols >= 4));
    }

    #[test]
    fn tab_order_and_active_index_are_stable_when_closing_the_active_tab() {
        let mut tabs = TabSet::new(Rect::new(0, 0, 1000, 600), CellSize::new(10, 10), 10, 5, 3);
        let first = tabs.active().unwrap().id();
        let second = tabs.add_tab();
        let third = tabs.add_tab();
        assert_eq!(tabs.active_index(), Some(2));
        assert!(tabs.select(1));
        assert_eq!(tabs.active().unwrap().id(), second);
        assert_eq!(tabs.close_active(), TabCloseResult::Closed(second));
        assert_eq!(tabs.active_index(), Some(1));
        assert_eq!(tabs.active().unwrap().id(), third);
        assert_eq!(tabs.tab_ids(), vec![first, third]);
        tabs.next();
        assert_eq!(tabs.active().unwrap().id(), first);
        tabs.prev();
        assert_eq!(tabs.active().unwrap().id(), third);
        assert!(!tabs.select(2));
        assert!(tabs.select(0));
        assert_eq!(tabs.close_active(), TabCloseResult::Closed(first));
        assert_eq!(tabs.close_active(), TabCloseResult::LastTab);
    }

    #[test]
    fn tab_focus_hit_test_selects_only_visible_pane() {
        let mut tabs = TabSet::new(Rect::new(0, 0, 300, 100), CellSize::new(10, 10), 10, 4, 2);
        let first = tabs.active().unwrap().active_pane();
        let second = tabs
            .active_mut()
            .unwrap()
            .split_focused(Direction::Horizontal)
            .unwrap();
        assert_eq!(
            tabs.active_mut().unwrap().focus_by_click(250, 20),
            Some(second)
        );
        assert_eq!(tabs.active().unwrap().active_pane(), second);
        assert_eq!(tabs.active_mut().unwrap().focus_by_click(145, 20), None);
        assert_eq!(tabs.active().unwrap().active_pane(), second);
        assert_ne!(first, second);
    }
}
