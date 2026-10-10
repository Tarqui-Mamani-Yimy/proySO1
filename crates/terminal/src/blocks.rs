//! Semantic command blocks derived from shell-integration events.

use crate::osc::{Event, ObservedEvent, SemanticMark};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockState {
    Prompt,
    Input,
    Running,
    Finished,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhaseBoundaries {
    pub prompt_start: Option<u64>,
    pub input_start: Option<u64>,
    pub output_start: Option<u64>,
    pub finished: Option<u64>,
    pub cancelled: Option<u64>,
}

/// Reserved anchor slots; T2 will populate these with terminal-grid positions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GridAnchors {
    pub prompt_start: Option<(i64, usize)>,
    pub input_start: Option<(i64, usize)>,
    pub output_start: Option<(i64, usize)>,
    pub end: Option<(i64, usize)>,
}

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
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BlockList {
    blocks: Vec<Block>,
    cwd: Option<WorkingDirectory>,
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

    pub fn consume(&mut self, events: &[ObservedEvent]) {
        for observed in events {
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
                            }
                            BlockState::Running => block.state = BlockState::Finished,
                            BlockState::Finished | BlockState::Cancelled => {}
                        }
                    }
                    let mut block = self.new_block();
                    block.boundaries.prompt_start = Some(observed.end_offset);
                    block.cwd = self.cwd.clone();
                    self.blocks.push(block);
                }
                Event::SemanticMark(SemanticMark::InputStart) => {
                    let current = self.blocks.last().map(|block| block.state);
                    if current == Some(BlockState::Prompt) {
                        let block = self.blocks.last_mut().expect("current block exists");
                        block.boundaries.input_start = Some(observed.end_offset);
                        block.state = BlockState::Input;
                    } else if current != Some(BlockState::Input) {
                        if current == Some(BlockState::Running) {
                            self.blocks.last_mut().expect("running block exists").state =
                                BlockState::Finished;
                        }
                        let mut block = self.new_block();
                        block.boundaries.input_start = Some(observed.end_offset);
                        block.state = BlockState::Input;
                        self.blocks.push(block);
                    }
                }
                Event::SemanticMark(SemanticMark::OutputStart) => {
                    let current = self.blocks.last().map(|block| block.state);
                    if current == Some(BlockState::Prompt) || current == Some(BlockState::Input) {
                        let block = self.blocks.last_mut().expect("current block exists");
                        block.boundaries.output_start = Some(observed.end_offset);
                        block.state = BlockState::Running;
                    } else if current != Some(BlockState::Running) {
                        let mut block = self.new_block();
                        block.boundaries.output_start = Some(observed.end_offset);
                        block.state = BlockState::Running;
                        self.blocks.push(block);
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
                        block.exit_status = *exit_status;
                        block.state = BlockState::Finished;
                    } else {
                        let mut block = self.new_block();
                        block.boundaries.finished = Some(observed.end_offset);
                        block.exit_status = *exit_status;
                        block.state = BlockState::Finished;
                        self.blocks.push(block);
                    }
                }
            }
        }
    }

    fn new_block(&self) -> Block {
        Block {
            id: self.blocks.len() as u64 + 1,
            boundaries: PhaseBoundaries::default(),
            exit_status: None,
            cwd: self.cwd.clone(),
            state: BlockState::Prompt,
            grid_anchors: GridAnchors::default(),
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
                "finished without output mark",
                vec![SemanticMark::CommandFinished { exit_status: None }],
                vec![BlockState::Finished],
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
    fn finish_without_output_mark_is_retained() {
        let list = blocks(&[mark(
            7,
            SemanticMark::CommandFinished {
                exit_status: Some(3),
            },
        )]);
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
}
