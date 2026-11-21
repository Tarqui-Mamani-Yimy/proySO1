//! Semantic command blocks derived from shell-integration events.

use std::time::SystemTime;

use crate::osc::{Event, ObservedEvent, SemanticMark};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockState {
    Prompt,
    Input,
    Running,
    Finished,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridScreen {
    Primary,
    Alternate,
}

/// A primary row is an absolute row: total evicted rows plus the retained
/// history and viewport index. It remains stable while scrollback grows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GridAnchor {
    pub line: i64,
    pub column: usize,
    pub screen: GridScreen,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhaseBoundaries {
    pub prompt_start: Option<u64>,
    pub input_start: Option<u64>,
    pub output_start: Option<u64>,
    pub finished: Option<u64>,
    pub cancelled: Option<u64>,
}

/// Wall-clock times of a block's command, taken when its marks were observed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockTimes {
    /// When the command started running (OSC 133 `C`).
    pub started: Option<SystemTime>,
    /// When the command finished (OSC 133 `D`, or an implicit finish).
    pub finished: Option<SystemTime>,
}

/// What a pane's shell is doing, as reported by its integration marks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneActivity {
    /// No shell-integration mark has been seen (or every block was discarded).
    Unknown,
    /// At a prompt or editing input; no command is running.
    Idle,
    /// A command is running (between OSC 133 `C` and `D`).
    Running,
}

impl PaneActivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Idle => "idle",
            Self::Running => "running",
        }
    }
}

/// Selects one block of a pane for metadata queries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockQuery {
    /// The newest block that ran a command (running or finished).
    Last,
    /// A block by its pane-local identifier.
    Id(u64),
}

/// Why a [`BlockQuery`] matched no block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockLookupError {
    /// The identifier was never assigned in this pane.
    NotFound(u64),
    /// The block existed but was discarded with its scrollback or by the block limit.
    Evicted(u64),
    /// No block has run a command yet.
    NoCommandBlock,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GridAnchors {
    pub prompt_start: Option<GridAnchor>,
    pub input_start: Option<GridAnchor>,
    pub output_start: Option<GridAnchor>,
    pub end: Option<GridAnchor>,
}

impl GridAnchors {
    fn clear_primary(&mut self) {
        for anchor in [
            &mut self.prompt_start,
            &mut self.input_start,
            &mut self.output_start,
            &mut self.end,
        ] {
            if anchor.is_some_and(|anchor| anchor.screen == GridScreen::Primary) {
                *anchor = None;
            }
        }
    }

    fn expire_primary_before(&mut self, boundary: u64) -> bool {
        let mut expired_any = false;
        for anchor in [
            &mut self.prompt_start,
            &mut self.input_start,
            &mut self.output_start,
            &mut self.end,
        ] {
            if anchor.is_some_and(|anchor| {
                anchor.screen == GridScreen::Primary && anchor.line < boundary as i64
            }) {
                *anchor = None;
                expired_any = true;
            }
        }
        expired_any
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn has_anchor(&self) -> bool {
        self.prompt_start.is_some()
            || self.input_start.is_some()
            || self.output_start.is_some()
            || self.end.is_some()
    }
}

const MAX_BLOCKS: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingDirectory {
    pub host: Vec<u8>,
    pub path: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Block {
    pub id: u64,
    pub boundaries: PhaseBoundaries,
    pub exit_status: Option<i32>,
    pub cwd: Option<WorkingDirectory>,
    pub state: BlockState,
    pub grid_anchors: GridAnchors,
    pub times: BlockTimes,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BlockList {
    blocks: Vec<Block>,
    cwd: Option<WorkingDirectory>,
    next_id: u64,
}

impl BlockList {
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn current(&self) -> Option<&Block> {
        self.blocks.iter().rev().find(|block| {
            matches!(
                block.state,
                BlockState::Prompt | BlockState::Input | BlockState::Running
            )
        })
    }

    /// Activity of the pane, taken from its newest block.
    pub fn activity(&self) -> PaneActivity {
        match self.blocks.last().map(|block| block.state) {
            None => PaneActivity::Unknown,
            Some(BlockState::Running) => PaneActivity::Running,
            Some(_) => PaneActivity::Idle,
        }
    }

    /// Exit status of the newest finished command, when it reported one.
    pub fn last_exit_status(&self) -> Option<i32> {
        self.blocks
            .iter()
            .rev()
            .find(|block| block.state == BlockState::Finished)
            .and_then(|block| block.exit_status)
    }

    pub fn consume(&mut self, events: &[ObservedEvent]) {
        self.consume_at(events, SystemTime::now());
    }

    /// Consumes events observed at `at`, which becomes the wall-clock time of
    /// any command start or finish they mark.
    pub fn consume_at(&mut self, events: &[ObservedEvent], at: SystemTime) {
        for observed in events {
            self.consume_event(observed, None, at);
        }
    }

    /// Finds a block, telling apart identifiers that were never assigned from
    /// blocks that were discarded.
    ///
    /// Block ids are assigned monotonically from 1, so `next_id` is the newest
    /// assigned id. An id in `1..=next_id` that is no longer retained was
    /// evicted (dropped with its scrollback rows or by the `MAX_BLOCKS` limit)
    /// and yields [`BlockLookupError::Evicted`]; `0` or any id above `next_id`
    /// was never assigned and yields [`BlockLookupError::NotFound`].
    pub fn find(&self, query: BlockQuery) -> Result<&Block, BlockLookupError> {
        match query {
            BlockQuery::Last => self
                .blocks
                .iter()
                .rev()
                .find(|block| matches!(block.state, BlockState::Running | BlockState::Finished))
                .ok_or(BlockLookupError::NoCommandBlock),
            BlockQuery::Id(id) => {
                if let Some(block) = self.blocks.iter().find(|block| block.id == id) {
                    Ok(block)
                } else if id == 0 || id > self.next_id {
                    Err(BlockLookupError::NotFound(id))
                } else {
                    Err(BlockLookupError::Evicted(id))
                }
            }
        }
    }

    pub(crate) fn consume_event(
        &mut self,
        observed: &ObservedEvent,
        anchor: Option<GridAnchor>,
        at: SystemTime,
    ) {
        match &observed.event {
            Event::CurrentDirectory { host, path } => {
                self.cwd = Some(WorkingDirectory {
                    host: host.clone(),
                    path: path.clone(),
                });
            }
            Event::SemanticMark(SemanticMark::PromptStart) => {
                if let Some(block) = self.blocks.last_mut() {
                    match block.state {
                        BlockState::Prompt | BlockState::Input => {
                            block.state = BlockState::Cancelled;
                            block.boundaries.cancelled = Some(observed.end_offset);
                            if block.grid_anchors.has_anchor() {
                                block.grid_anchors.end = anchor;
                            }
                        }
                        BlockState::Running => {
                            block.state = BlockState::Finished;
                            block.boundaries.finished = Some(observed.end_offset);
                            block.times.finished = Some(at);
                            block.exit_status = None;
                            if block.grid_anchors.has_anchor() {
                                block.grid_anchors.end = anchor;
                            }
                        }
                        BlockState::Finished | BlockState::Cancelled => {}
                    }
                }
                let mut block = self.new_block();
                block.boundaries.prompt_start = Some(observed.end_offset);
                block.grid_anchors.prompt_start = anchor;
                block.cwd = self.cwd.clone();
                self.push(block);
            }
            Event::SemanticMark(SemanticMark::InputStart) => {
                let current = self.blocks.last().map(|block| block.state);
                if current == Some(BlockState::Prompt) {
                    let block = self.blocks.last_mut().expect("current block exists");
                    block.boundaries.input_start = Some(observed.end_offset);
                    block.grid_anchors.input_start = anchor;
                    block.state = BlockState::Input;
                } else if current != Some(BlockState::Input) {
                    if current == Some(BlockState::Running) {
                        let block = self.blocks.last_mut().expect("running block exists");
                        block.state = BlockState::Finished;
                        block.boundaries.finished = Some(observed.end_offset);
                        block.times.finished = Some(at);
                        if block.grid_anchors.has_anchor() {
                            block.grid_anchors.end = anchor;
                        }
                    }
                    let mut block = self.new_block();
                    block.boundaries.input_start = Some(observed.end_offset);
                    block.grid_anchors.input_start = anchor;
                    block.state = BlockState::Input;
                    self.push(block);
                }
            }
            Event::SemanticMark(SemanticMark::OutputStart) => {
                let current = self.blocks.last().map(|block| block.state);
                if current == Some(BlockState::Prompt) || current == Some(BlockState::Input) {
                    let block = self.blocks.last_mut().expect("current block exists");
                    block.boundaries.output_start = Some(observed.end_offset);
                    block.grid_anchors.output_start = anchor;
                    block.state = BlockState::Running;
                    block.times.started = Some(at);
                } else if current != Some(BlockState::Running) {
                    let mut block = self.new_block();
                    block.boundaries.output_start = Some(observed.end_offset);
                    block.grid_anchors.output_start = anchor;
                    block.state = BlockState::Running;
                    block.times.started = Some(at);
                    self.push(block);
                }
            }
            Event::SemanticMark(SemanticMark::CommandFinished { exit_status }) => {
                let current = self.blocks.last().map(|block| block.state);
                if matches!(
                    current,
                    Some(BlockState::Prompt | BlockState::Input | BlockState::Running)
                ) {
                    let block = self.blocks.last_mut().expect("current block exists");
                    block.boundaries.finished = Some(observed.end_offset);
                    block.times.finished = Some(at);
                    block.exit_status = *exit_status;
                    block.state = BlockState::Finished;
                    if block.grid_anchors.has_anchor() {
                        block.grid_anchors.end = anchor;
                    }
                }
            }
        }
    }

    /// Resize/reflow has no stable row mapping in alacritty_terminal 0.26.
    pub(crate) fn invalidate_anchors(&mut self) {
        for block in &mut self.blocks {
            block.grid_anchors.clear();
        }
    }

    pub(crate) fn invalidate_primary_anchors(&mut self) {
        for block in &mut self.blocks {
            block.grid_anchors.clear_primary();
        }
    }

    /// Expire primary anchors below the exact evicted-row boundary and discard
    /// completed blocks that no longer have any surviving grid anchors.
    pub(crate) fn expire_primary_anchors_before(&mut self, boundary: u64) {
        self.blocks.retain_mut(|block| {
            let expired = block.grid_anchors.expire_primary_before(boundary);
            let completed = matches!(block.state, BlockState::Finished | BlockState::Cancelled);
            !(expired && completed && !block.grid_anchors.has_anchor())
        });
    }

    pub(crate) fn push(&mut self, block: Block) {
        self.blocks.push(block);
        if self.blocks.len() > MAX_BLOCKS {
            let removable = self
                .blocks
                .iter()
                .position(|block| {
                    !matches!(
                        block.state,
                        BlockState::Prompt | BlockState::Input | BlockState::Running
                    )
                })
                .unwrap_or(0);
            self.blocks.remove(removable);
        }
    }

    fn new_block(&mut self) -> Block {
        self.next_id = self.next_id.saturating_add(1);
        Block {
            id: self.next_id,
            boundaries: PhaseBoundaries::default(),
            exit_status: None,
            cwd: self.cwd.clone(),
            state: BlockState::Prompt,
            grid_anchors: GridAnchors::default(),
            times: BlockTimes::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(offset: u64, mark: SemanticMark) -> ObservedEvent {
        ObservedEvent {
            end_offset: offset,
            event: Event::SemanticMark(mark),
        }
    }

    fn cwd(offset: u64, host: &[u8], path: &[u8]) -> ObservedEvent {
        ObservedEvent {
            end_offset: offset,
            event: Event::CurrentDirectory {
                host: host.to_vec(),
                path: path.to_vec(),
            },
        }
    }

    fn blocks(events: &[ObservedEvent]) -> BlockList {
        let mut blocks = BlockList::default();
        blocks.consume(events);
        blocks
    }

    #[test]
    fn common_phase_sequences_follow_the_transition_table() {
        let scenarios = [
            (
                "prompt",
                vec![SemanticMark::PromptStart],
                vec![BlockState::Prompt],
            ),
            (
                "input",
                vec![SemanticMark::PromptStart, SemanticMark::InputStart],
                vec![BlockState::Input],
            ),
            (
                "running without prompt marks",
                vec![SemanticMark::OutputStart],
                vec![BlockState::Running],
            ),
            (
                "finished marker without an open block is ignored",
                vec![SemanticMark::CommandFinished { exit_status: None }],
                vec![],
            ),
            (
                "cancelled input followed by fresh prompt",
                vec![
                    SemanticMark::PromptStart,
                    SemanticMark::InputStart,
                    SemanticMark::PromptStart,
                ],
                vec![BlockState::Cancelled, BlockState::Prompt],
            ),
            (
                "empty accepted command",
                vec![
                    SemanticMark::PromptStart,
                    SemanticMark::OutputStart,
                    SemanticMark::CommandFinished {
                        exit_status: Some(0),
                    },
                ],
                vec![BlockState::Finished],
            ),
        ];

        for (name, marks, expected_states) in scenarios {
            let events: Vec<_> = marks
                .into_iter()
                .enumerate()
                .map(|(index, semantic_mark)| mark(index as u64 + 1, semantic_mark))
                .collect();
            let actual: Vec<_> = blocks(&events)
                .blocks()
                .iter()
                .map(|block| block.state)
                .collect();
            assert_eq!(actual, expected_states, "scenario: {name}");
        }
    }

    #[test]
    fn normal_command_records_each_boundary_and_status() {
        let list = blocks(&[
            mark(10, SemanticMark::PromptStart),
            mark(20, SemanticMark::InputStart),
            mark(30, SemanticMark::OutputStart),
            mark(
                40,
                SemanticMark::CommandFinished {
                    exit_status: Some(7),
                },
            ),
        ]);
        assert_eq!(list.blocks().len(), 1);
        let block = &list.blocks()[0];
        assert_eq!(block.id, 1);
        assert_eq!(block.boundaries.prompt_start, Some(10));
        assert_eq!(block.boundaries.input_start, Some(20));
        assert_eq!(block.boundaries.output_start, Some(30));
        assert_eq!(block.boundaries.finished, Some(40));
        assert_eq!(block.exit_status, Some(7));
        assert_eq!(block.state, BlockState::Finished);
    }

    #[test]
    fn cancelled_input_closes_old_block_before_next_prompt() {
        let list = blocks(&[
            mark(1, SemanticMark::PromptStart),
            mark(2, SemanticMark::InputStart),
            mark(3, SemanticMark::PromptStart),
        ]);
        assert_eq!(list.blocks().len(), 2);
        assert_eq!(list.blocks()[0].state, BlockState::Cancelled);
        assert_eq!(list.blocks()[0].boundaries.cancelled, Some(3));
        assert_eq!(list.blocks()[1].state, BlockState::Prompt);
    }

    #[test]
    fn empty_accepted_line_needs_no_input_mark() {
        let list = blocks(&[
            mark(4, SemanticMark::PromptStart),
            mark(5, SemanticMark::OutputStart),
            mark(
                6,
                SemanticMark::CommandFinished {
                    exit_status: Some(0),
                },
            ),
        ]);
        assert_eq!(list.blocks().len(), 1);
        let block = &list.blocks()[0];
        assert_eq!(block.boundaries.input_start, None);
        assert_eq!(block.state, BlockState::Finished);
        assert_eq!(block.exit_status, Some(0));
    }

    #[test]
    fn accepted_multiline_input_remains_one_block_until_completion() {
        // PS2 input adds no integration mark; the primary line's A/B and the
        // accepted command's C/D still delimit the single multiline command.
        let list = blocks(&[
            mark(1, SemanticMark::PromptStart),
            mark(2, SemanticMark::InputStart),
            mark(3, SemanticMark::OutputStart),
            mark(
                4,
                SemanticMark::CommandFinished {
                    exit_status: Some(0),
                },
            ),
        ]);
        assert_eq!(list.blocks().len(), 1);
        assert_eq!(list.blocks()[0].state, BlockState::Finished);
        assert_eq!(list.blocks()[0].boundaries.input_start, Some(2));
        assert_eq!(list.blocks()[0].boundaries.output_start, Some(3));
        assert_eq!(list.blocks()[0].exit_status, Some(0));
    }

    #[test]
    fn block_times_record_command_start_and_finish_wall_clock() {
        use std::time::{Duration, UNIX_EPOCH};
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        let mut list = BlockList::default();
        list.consume_at(&[mark(1, SemanticMark::PromptStart)], at(10));
        list.consume_at(&[mark(2, SemanticMark::InputStart)], at(11));
        list.consume_at(&[mark(3, SemanticMark::OutputStart)], at(12));
        assert_eq!(list.blocks()[0].times.started, Some(at(12)));
        assert_eq!(list.blocks()[0].times.finished, None);
        list.consume_at(
            &[mark(
                4,
                SemanticMark::CommandFinished {
                    exit_status: Some(0),
                },
            )],
            at(15),
        );
        assert_eq!(list.blocks()[0].times.finished, Some(at(15)));

        // A running block closed implicitly by the next prompt also finishes.
        list.consume_at(&[mark(5, SemanticMark::OutputStart)], at(20));
        list.consume_at(&[mark(6, SemanticMark::PromptStart)], at(23));
        let implicit = &list.blocks()[1];
        assert_eq!(implicit.state, BlockState::Finished);
        assert_eq!(implicit.times.started, Some(at(20)));
        assert_eq!(implicit.times.finished, Some(at(23)));
    }

    #[test]
    fn pane_activity_follows_the_newest_block_and_keeps_the_last_exit_status() {
        let mut list = BlockList::default();
        assert_eq!(list.activity(), PaneActivity::Unknown);
        assert_eq!(list.last_exit_status(), None);
        list.consume(&[mark(1, SemanticMark::PromptStart)]);
        assert_eq!(list.activity(), PaneActivity::Idle);
        list.consume(&[
            mark(2, SemanticMark::InputStart),
            mark(3, SemanticMark::OutputStart),
        ]);
        assert_eq!(list.activity(), PaneActivity::Running);
        list.consume(&[mark(
            4,
            SemanticMark::CommandFinished {
                exit_status: Some(7),
            },
        )]);
        assert_eq!(list.activity(), PaneActivity::Idle);
        assert_eq!(list.last_exit_status(), Some(7));
        // A new prompt and a cancelled line keep the previous command's status.
        list.consume(&[
            mark(5, SemanticMark::PromptStart),
            mark(6, SemanticMark::PromptStart),
        ]);
        assert_eq!(list.activity(), PaneActivity::Idle);
        assert_eq!(list.last_exit_status(), Some(7));
        assert_eq!(
            [
                PaneActivity::Unknown.as_str(),
                PaneActivity::Idle.as_str(),
                PaneActivity::Running.as_str()
            ],
            ["unknown", "idle", "running"]
        );
    }

    #[test]
    fn find_distinguishes_last_command_unknown_and_evicted_ids() {
        let mut list = blocks(&[
            mark(1, SemanticMark::PromptStart),
            mark(2, SemanticMark::OutputStart),
            mark(
                3,
                SemanticMark::CommandFinished {
                    exit_status: Some(1),
                },
            ),
            mark(4, SemanticMark::PromptStart),
        ]);
        // The trailing prompt has no command, so `last` is the finished block.
        assert_eq!(list.find(BlockQuery::Last).map(|block| block.id), Ok(1));
        assert_eq!(list.find(BlockQuery::Id(2)).map(|block| block.id), Ok(2));
        assert_eq!(
            list.find(BlockQuery::Id(9)),
            Err(BlockLookupError::NotFound(9))
        );
        assert_eq!(
            list.find(BlockQuery::Id(0)),
            Err(BlockLookupError::NotFound(0))
        );
        list.expire_primary_anchors_before(u64::MAX);
        list.blocks.retain(|block| block.id != 1);
        assert_eq!(
            list.find(BlockQuery::Id(1)),
            Err(BlockLookupError::Evicted(1))
        );
        assert_eq!(
            BlockList::default().find(BlockQuery::Last),
            Err(BlockLookupError::NoCommandBlock)
        );
    }

    #[test]
    fn find_treats_the_newest_assigned_id_as_the_evicted_boundary() {
        let mut list = blocks(&[
            mark(1, SemanticMark::PromptStart),
            mark(2, SemanticMark::OutputStart),
            mark(
                3,
                SemanticMark::CommandFinished {
                    exit_status: Some(0),
                },
            ),
        ]);
        // Block 1 is the newest assigned id (`next_id`) and is still retained.
        assert_eq!(list.find(BlockQuery::Id(1)).map(|block| block.id), Ok(1));
        // One past the newest assigned id was never assigned.
        assert_eq!(
            list.find(BlockQuery::Id(2)),
            Err(BlockLookupError::NotFound(2))
        );
        // Once the newest block is gone its id reads as evicted, not unknown.
        list.blocks.clear();
        assert_eq!(
            list.find(BlockQuery::Id(1)),
            Err(BlockLookupError::Evicted(1))
        );
        assert_eq!(
            list.find(BlockQuery::Id(2)),
            Err(BlockLookupError::NotFound(2))
        );
    }

    #[test]
    fn find_reports_blocks_dropped_by_the_block_limit_as_evicted() {
        let commands = MAX_BLOCKS as u64 + 1;
        let events: Vec<_> = (0..commands)
            .flat_map(|index| {
                let offset = index * 3;
                [
                    mark(offset + 1, SemanticMark::PromptStart),
                    mark(offset + 2, SemanticMark::OutputStart),
                    mark(
                        offset + 3,
                        SemanticMark::CommandFinished {
                            exit_status: Some(0),
                        },
                    ),
                ]
            })
            .collect();
        let list = blocks(&events);

        assert_eq!(list.blocks().len(), MAX_BLOCKS);
        assert_eq!(
            list.find(BlockQuery::Id(1)),
            Err(BlockLookupError::Evicted(1))
        );
        assert_eq!(list.find(BlockQuery::Id(2)).map(|block| block.id), Ok(2));
        assert_eq!(
            list.find(BlockQuery::Id(commands)).map(|block| block.id),
            Ok(commands)
        );
        assert_eq!(
            list.find(BlockQuery::Id(commands + 1)),
            Err(BlockLookupError::NotFound(commands + 1))
        );
    }

    #[test]
    fn output_without_prompt_or_input_and_duplicate_output_are_tolerated() {
        let list = blocks(&[
            mark(1, SemanticMark::OutputStart),
            mark(2, SemanticMark::OutputStart),
            mark(
                3,
                SemanticMark::CommandFinished {
                    exit_status: Some(2),
                },
            ),
        ]);
        assert_eq!(list.blocks().len(), 1);
        assert_eq!(list.blocks()[0].state, BlockState::Finished);
        assert_eq!(list.blocks()[0].boundaries.output_start, Some(1));
        assert_eq!(list.blocks()[0].boundaries.finished, Some(3));
        assert_eq!(list.blocks()[0].exit_status, Some(2));
    }

    #[test]
    fn duplicate_prompt_cancels_the_unaccepted_prompt() {
        let list = blocks(&[
            mark(5, SemanticMark::PromptStart),
            mark(6, SemanticMark::PromptStart),
        ]);
        assert_eq!(list.blocks().len(), 2);
        assert_eq!(list.blocks()[0].state, BlockState::Cancelled);
        assert_eq!(list.blocks()[0].boundaries.cancelled, Some(6));
        assert_eq!(list.blocks()[1].state, BlockState::Prompt);
    }

    #[test]
    fn finish_without_output_mark_closes_an_open_block() {
        let list = blocks(&[
            mark(6, SemanticMark::PromptStart),
            mark(
                7,
                SemanticMark::CommandFinished {
                    exit_status: Some(3),
                },
            ),
        ]);
        assert_eq!(list.blocks().len(), 1);
        assert_eq!(list.blocks()[0].boundaries.output_start, None);
        assert_eq!(list.blocks()[0].state, BlockState::Finished);
        assert_eq!(list.blocks()[0].exit_status, Some(3));
    }

    #[test]
    fn malformed_status_finishes_with_unknown_exit_status() {
        let list = blocks(&[
            mark(1, SemanticMark::PromptStart),
            mark(2, SemanticMark::OutputStart),
            mark(3, SemanticMark::CommandFinished { exit_status: None }),
        ]);
        assert_eq!(list.blocks()[0].state, BlockState::Finished);
        assert_eq!(list.blocks()[0].exit_status, None);
    }

    #[test]
    fn cwd_is_snapshotted_when_each_prompt_starts() {
        let list = blocks(&[
            cwd(1, b"host", b"/work"),
            mark(2, SemanticMark::PromptStart),
            mark(3, SemanticMark::OutputStart),
            mark(
                4,
                SemanticMark::CommandFinished {
                    exit_status: Some(0),
                },
            ),
            cwd(5, b"host", b"/tmp"),
            mark(6, SemanticMark::PromptStart),
        ]);
        assert_eq!(list.blocks()[0].cwd.as_ref().unwrap().path, b"/work");
        assert_eq!(list.blocks()[1].cwd.as_ref().unwrap().path, b"/tmp");
    }

    #[test]
    fn no_shell_integration_events_produce_no_blocks() {
        assert!(blocks(&[]).blocks().is_empty());
        assert!(blocks(&[cwd(1, b"host", b"/work")]).blocks().is_empty());
    }

    #[test]
    fn command_finished_without_an_open_block_does_not_create_a_phantom() {
        let list = blocks(&[mark(
            7,
            SemanticMark::CommandFinished {
                exit_status: Some(3),
            },
        )]);
        assert!(list.blocks().is_empty());
    }

    #[test]
    fn interrupted_running_block_records_its_implicit_end_boundary() {
        let list = blocks(&[
            mark(1, SemanticMark::PromptStart),
            mark(2, SemanticMark::OutputStart),
            mark(3, SemanticMark::PromptStart),
        ]);
        let old = &list.blocks()[0];
        assert_eq!(old.state, BlockState::Finished);
        assert_eq!(old.boundaries.finished, Some(3));
        assert_eq!(old.exit_status, None);
    }

    #[test]
    fn block_history_is_bounded_and_keeps_monotonic_ids() {
        let mut list = BlockList::default();
        for i in 0..2048 {
            let base = i * 2;
            list.consume(&[
                mark(base, SemanticMark::PromptStart),
                mark(
                    base + 1,
                    SemanticMark::CommandFinished { exit_status: None },
                ),
            ]);
        }
        assert!(list.blocks().len() <= 1024);
        assert!(list.blocks().last().unwrap().id > 1024);
    }
}
