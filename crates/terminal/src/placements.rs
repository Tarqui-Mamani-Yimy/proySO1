use so1_terminal::blocks::{GridAnchor, GridScreen};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellSize {
    pub columns: usize,
    pub rows: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Placement creation is consumed by T7 rendering.
pub struct Placement {
    pub image_id: u32,
    pub placement_id: Option<u32>,
    pub anchor: GridAnchor,
    pub source: SourceRect,
    pub cells: CellSize,
    pub pixel_offset: (u32, u32),
    pub z: i32,
    insertion: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Renderer-facing viewport query is intentionally headless in T6.
pub struct Viewport {
    pub screen: GridScreen,
    pub top_row: i64,
    pub rows: usize,
    pub columns: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Renderer-facing projection is intentionally headless in T6.
pub struct VisiblePlacement {
    pub placement: Placement,
    pub row: usize,
    pub column: usize,
    pub rows: usize,
    pub columns: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteSelector {
    All,
    Image {
        id: u32,
        placement_id: Option<u32>,
    },
    ImageNumber {
        number: u32,
        placement_id: Option<u32>,
    },
    CursorCell,
    Cell {
        column: usize,
        row: i64,
    },
    Column {
        column: usize,
    },
    Row {
        row: i64,
    },
    ZIndex {
        z: i32,
    },
}

#[derive(Default)]
pub struct PlacementStore {
    placements: Vec<Placement>,
    images: BTreeMap<u32, ImageInfo>,
    next_insertion: u64,
}

#[derive(Clone, Copy)]
struct ImageInfo {
    number: Option<u32>,
    insertion: u64,
}

impl PlacementStore {
    pub fn register_image(&mut self, id: u32, number: Option<u32>) {
        self.images.insert(
            id,
            ImageInfo {
                number,
                insertion: self.next_insertion,
            },
        );
        self.next_insertion = self.next_insertion.saturating_add(1);
        self.placements.retain(|placement| placement.image_id != id);
    }

    #[allow(dead_code)] // Display actions stay disabled until T7 can render placements.
    pub fn place(&mut self, mut placement: Placement) {
        self.register_if_missing(placement.image_id);
        if placement.image_id != 0
            && let Some(placement_id) = placement.placement_id.filter(|id| *id != 0)
        {
            self.placements.retain(|existing| {
                existing.image_id != placement.image_id
                    || existing.placement_id != Some(placement_id)
            });
        }
        placement.insertion = self.next_insertion;
        self.next_insertion = self.next_insertion.saturating_add(1);
        self.placements.push(placement);
    }

    fn register_if_missing(&mut self, id: u32) {
        if !self.images.contains_key(&id) {
            self.images.insert(
                id,
                ImageInfo {
                    number: None,
                    insertion: self.next_insertion,
                },
            );
            self.next_insertion = self.next_insertion.saturating_add(1);
        }
    }

    #[allow(dead_code)] // Read-only inspection supports the renderer and tests.
    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    #[allow(dead_code)] // Image liveness is queried by tests and future display integration.
    pub fn has_image(&self, id: u32) -> bool {
        self.images.contains_key(&id)
    }

    pub fn prune_evicted_primary(&mut self, boundary: u64) {
        self.placements.retain(|placement| {
            placement.anchor.screen != GridScreen::Primary
                || placement.anchor.line >= boundary as i64
        });
    }

    pub fn screen_changed(&mut self, previous: GridScreen, next: GridScreen) {
        if previous == GridScreen::Alternate && next == GridScreen::Primary {
            self.placements
                .retain(|placement| placement.anchor.screen != GridScreen::Alternate);
        }
    }

    pub fn resize(
        &mut self,
        old_columns: usize,
        _old_rows: usize,
        new_columns: usize,
        _new_rows: usize,
    ) {
        if old_columns != new_columns {
            self.placements.clear();
        }
    }

    pub fn retain_images(&mut self, mut exists: impl FnMut(u32) -> bool) {
        self.images.retain(|id, _| exists(*id));
        self.placements
            .retain(|placement| self.images.contains_key(&placement.image_id));
    }

    /// Applies a delete selector and returns image ids whose decoded data can be freed.
    pub fn delete(
        &mut self,
        selector: DeleteSelector,
        free_data: bool,
        cursor: GridAnchor,
        active_screen: GridScreen,
        viewport: Viewport,
    ) -> Vec<u32> {
        let target_image = match selector {
            DeleteSelector::ImageNumber { number, .. } => self
                .images
                .iter()
                .filter(|(_, info)| info.number == Some(number))
                .max_by_key(|(_, info)| info.insertion)
                .map(|(id, _)| *id),
            _ => None,
        };
        let direct_image = match selector {
            DeleteSelector::Image { id, .. } => Some(id),
            _ => target_image,
        };
        let matches = |placement: &Placement| match selector {
            DeleteSelector::All => {
                placement.anchor.screen == active_screen && intersects_viewport(placement, viewport)
            }
            DeleteSelector::Image { id, placement_id } => {
                placement.image_id == id
                    && placement_id.is_none_or(|p| placement.placement_id == Some(p))
            }
            DeleteSelector::ImageNumber { placement_id, .. } => {
                target_image == Some(placement.image_id)
                    && placement_id.is_none_or(|p| placement.placement_id == Some(p))
            }
            DeleteSelector::CursorCell => {
                placement.anchor.screen == cursor.screen
                    && intersects_cell(placement, cursor.column, cursor.line)
            }
            DeleteSelector::Cell { column, row } => {
                placement.anchor.screen == active_screen && intersects_cell(placement, column, row)
            }
            DeleteSelector::Column { column } => {
                placement.anchor.screen == active_screen
                    && placement.anchor.column <= column
                    && column
                        < placement
                            .anchor
                            .column
                            .saturating_add(placement.cells.columns)
            }
            DeleteSelector::Row { row } => {
                placement.anchor.screen == active_screen
                    && placement.anchor.line <= row
                    && row
                        < placement
                            .anchor
                            .line
                            .saturating_add(placement.cells.rows as i64)
            }
            DeleteSelector::ZIndex { z } => placement.z == z,
        };
        let removed: Vec<u32> = self
            .placements
            .iter()
            .filter(|placement| matches(placement))
            .map(|placement| placement.image_id)
            .collect();
        self.placements.retain(|placement| !matches(placement));
        if !free_data {
            return Vec::new();
        }
        let mut candidates: Vec<u32> = removed;
        if let Some(id) = direct_image {
            candidates.push(id);
        }
        candidates
            .into_iter()
            .filter(|id| !self.placements.iter().any(|p| p.image_id == *id))
            .fold(Vec::new(), |mut unique, id| {
                if self.images.remove(&id).is_some() && !unique.contains(&id) {
                    unique.push(id);
                }
                unique
            })
    }

    #[allow(dead_code)] // Renderer-facing query is intentionally not called before T7.
    pub fn visible(&self, viewport: Viewport) -> Vec<VisiblePlacement> {
        let end_row = viewport.top_row.saturating_add(viewport.rows as i64);
        let mut visible: Vec<_> = self
            .placements
            .iter()
            .filter(|placement| placement.anchor.screen == viewport.screen)
            .filter_map(|placement| {
                let row_start = placement.anchor.line.max(viewport.top_row);
                let row_end = placement
                    .anchor
                    .line
                    .saturating_add(placement.cells.rows as i64)
                    .min(end_row);
                let col_start = placement.anchor.column;
                let col_end = col_start
                    .saturating_add(placement.cells.columns)
                    .min(viewport.columns);
                if row_start >= row_end || col_start >= col_end {
                    return None;
                }
                Some(VisiblePlacement {
                    placement: placement.clone(),
                    row: (row_start - viewport.top_row) as usize,
                    column: col_start,
                    rows: (row_end - row_start) as usize,
                    columns: col_end - col_start,
                })
            })
            .collect();
        visible.sort_by_key(|entry| (entry.placement.z, entry.placement.insertion));
        visible
    }
}

fn intersects_cell(placement: &Placement, column: usize, row: i64) -> bool {
    placement.anchor.line <= row
        && row
            < placement
                .anchor
                .line
                .saturating_add(placement.cells.rows as i64)
        && placement.anchor.column <= column
        && column
            < placement
                .anchor
                .column
                .saturating_add(placement.cells.columns)
}

fn intersects_viewport(placement: &Placement, viewport: Viewport) -> bool {
    let bottom = viewport.top_row.saturating_add(viewport.rows as i64);
    placement.anchor.line < bottom
        && placement
            .anchor
            .line
            .saturating_add(placement.cells.rows as i64)
            > viewport.top_row
        && placement.anchor.column < viewport.columns
        && placement
            .anchor
            .column
            .saturating_add(placement.cells.columns)
            > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(id: u32, p: Option<u32>, screen: GridScreen, row: i64, col: usize) -> Placement {
        Placement {
            image_id: id,
            placement_id: p,
            anchor: GridAnchor {
                line: row,
                column: col,
                screen,
            },
            source: SourceRect {
                x: 0,
                y: 0,
                width: 20,
                height: 20,
            },
            cells: CellSize {
                columns: 2,
                rows: 2,
            },
            pixel_offset: (0, 0),
            z: 0,
            insertion: 0,
        }
    }

    fn full_viewport(screen: GridScreen) -> Viewport {
        Viewport {
            screen,
            top_row: 0,
            rows: 24,
            columns: 80,
        }
    }

    #[test]
    fn placement_uses_stable_row_and_replaces_same_image_and_placement_id() {
        let mut store = PlacementStore::default();
        store.place(placement(7, Some(3), GridScreen::Primary, 41, 5));
        store.place(placement(7, Some(3), GridScreen::Primary, 44, 2));

        let items = store.visible(Viewport {
            screen: GridScreen::Primary,
            top_row: 44,
            rows: 4,
            columns: 10,
        });
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].placement.anchor.line, 44);
        assert_eq!(items[0].placement.anchor.column, 2);
    }

    #[test]
    fn cursor_anchor_reuses_the_model_absolute_row_across_scrollback_growth() {
        let mut model = so1_terminal::model::Model::new(8, 2);
        let anchor = model.cursor_anchor();
        let mut store = PlacementStore::default();
        store.place(placement(
            12,
            Some(4),
            anchor.screen,
            anchor.line,
            anchor.column,
        ));
        model.feed(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        assert!(model.evicted_rows() == 0);
        assert_eq!(store.placements()[0].anchor, anchor);
        assert_eq!(model.cursor_anchor().line, anchor.line + 4);
    }

    #[test]
    fn delete_modes_cover_placements_and_uppercase_frees_unreferenced_images() {
        let selectors = [
            DeleteSelector::Image {
                id: 1,
                placement_id: None,
            },
            DeleteSelector::Image {
                id: 2,
                placement_id: Some(1),
            },
            DeleteSelector::ImageNumber {
                number: 103,
                placement_id: None,
            },
            DeleteSelector::CursorCell,
            DeleteSelector::Cell { column: 5, row: 5 },
            DeleteSelector::Column { column: 5 },
            DeleteSelector::Row { row: 5 },
            DeleteSelector::ZIndex { z: 0 },
            DeleteSelector::All,
        ];
        for (index, selector) in selectors.into_iter().enumerate() {
            let target = index as u32 + 1;
            let mut store = PlacementStore::default();
            store.register_image(target, Some(target + 100));
            store.place(placement(target, Some(1), GridScreen::Primary, 5, 5));
            let cursor = GridAnchor {
                line: 5,
                column: 5,
                screen: GridScreen::Primary,
            };
            let freed = store.delete(
                selector,
                true,
                cursor,
                GridScreen::Primary,
                full_viewport(GridScreen::Primary),
            );
            assert_eq!(freed, [target], "image {target} should be freed");
        }
    }

    #[test]
    fn delete_all_only_removes_placements_intersecting_the_viewport() {
        let mut store = PlacementStore::default();
        store.place(placement(1, None, GridScreen::Primary, 2, 1));
        store.place(placement(2, None, GridScreen::Primary, 50, 1));
        let freed = store.delete(
            DeleteSelector::All,
            false,
            GridAnchor {
                line: 0,
                column: 0,
                screen: GridScreen::Primary,
            },
            GridScreen::Primary,
            Viewport {
                screen: GridScreen::Primary,
                top_row: 0,
                rows: 5,
                columns: 10,
            },
        );
        assert!(freed.is_empty());
        assert_eq!(store.placements().len(), 1);
        assert_eq!(store.placements()[0].image_id, 2);
    }

    #[test]
    fn scrollback_eviction_drops_old_placements_but_not_unplaced_images() {
        let mut store = PlacementStore::default();
        store.register_image(1, None);
        store.register_image(2, None);
        store.place(placement(1, None, GridScreen::Primary, 4, 0));
        store.prune_evicted_primary(5);
        assert!(store.placements().is_empty());
        assert!(store.has_image(1));
        assert!(store.has_image(2));
    }

    #[test]
    fn image_quota_eviction_removes_placements_for_only_the_evicted_image() {
        let mut store = PlacementStore::default();
        store.place(placement(1, None, GridScreen::Primary, 1, 0));
        store.place(placement(2, None, GridScreen::Primary, 2, 0));
        store.retain_images(|id| id == 2);
        assert_eq!(store.placements().len(), 1);
        assert_eq!(store.placements()[0].image_id, 2);
    }

    #[test]
    fn lowercase_delete_keeps_image_data_and_uppercase_delete_frees_only_unreferenced_data() {
        let mut store = PlacementStore::default();
        store.register_image(1, None);
        store.place(placement(1, Some(1), GridScreen::Primary, 5, 5));
        store.place(placement(1, Some(2), GridScreen::Primary, 8, 2));
        let cursor = GridAnchor {
            line: 5,
            column: 5,
            screen: GridScreen::Primary,
        };

        assert!(
            store
                .delete(
                    DeleteSelector::Image {
                        id: 1,
                        placement_id: Some(1)
                    },
                    false,
                    cursor,
                    GridScreen::Primary,
                    full_viewport(GridScreen::Primary),
                )
                .is_empty()
        );
        assert!(store.has_image(1));
        assert_eq!(store.placements().len(), 1);
        assert!(
            store
                .delete(
                    DeleteSelector::Image {
                        id: 1,
                        placement_id: Some(2)
                    },
                    true,
                    cursor,
                    GridScreen::Primary,
                    full_viewport(GridScreen::Primary),
                )
                .contains(&1)
        );
        assert!(!store.has_image(1));
    }

    #[test]
    fn image_number_delete_targets_the_newest_image_with_that_number() {
        let mut store = PlacementStore::default();
        for id in [10, 11] {
            store.register_image(id, Some(77));
            store.place(placement(id, None, GridScreen::Primary, 0, 0));
        }
        let freed = store.delete(
            DeleteSelector::ImageNumber {
                number: 77,
                placement_id: None,
            },
            true,
            GridAnchor {
                line: 0,
                column: 0,
                screen: GridScreen::Primary,
            },
            GridScreen::Primary,
            full_viewport(GridScreen::Primary),
        );
        assert_eq!(freed, [11]);
        assert!(store.has_image(10));
    }

    #[test]
    fn alternate_screen_exit_discards_only_alternate_placements() {
        let mut store = PlacementStore::default();
        store.place(placement(1, None, GridScreen::Primary, 0, 0));
        store.place(placement(2, None, GridScreen::Alternate, 0, 0));
        store.screen_changed(GridScreen::Alternate, GridScreen::Primary);
        assert_eq!(store.placements().len(), 1);
        assert_eq!(store.placements()[0].image_id, 1);
    }

    #[test]
    fn width_resize_invalidates_but_height_only_resize_preserves_placements() {
        let mut store = PlacementStore::default();
        store.place(placement(1, None, GridScreen::Primary, 0, 0));
        store.resize(80, 24, 80, 40);
        assert_eq!(store.placements().len(), 1);
        store.resize(80, 40, 90, 40);
        assert!(store.placements().is_empty());
    }

    #[test]
    fn viewport_query_orders_by_z_then_insertion_and_clips_to_pane() {
        let mut store = PlacementStore::default();
        store.place(placement(3, None, GridScreen::Primary, 9, 9));
        let mut above = placement(2, None, GridScreen::Primary, 10, 1);
        above.z = 1;
        store.place(above);
        let mut below = placement(1, None, GridScreen::Primary, 10, 0);
        below.z = -1;
        store.place(below);

        let visible = store.visible(Viewport {
            screen: GridScreen::Primary,
            top_row: 10,
            rows: 1,
            columns: 2,
        });
        assert_eq!(
            visible
                .iter()
                .map(|v| v.placement.image_id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!((visible[0].rows, visible[0].columns), (1, 2));
    }
}
