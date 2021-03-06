//! Generate control flow graphs out of a program term.
//!
//! The generated graphs follow some basic principles:
//! * **Nodes** denote specific (abstract) points in time during program execution,
//! i.e. information does not change on a node.
//! So a basic block itself is not a node,
//! but the points in time before and after execution of the basic block can be nodes.
//! * **Edges** denote either transitions between the points in time of their start and end nodes during program execution
//! or they denote (artificial) information flow between nodes. See the `CRCallStub` edges of interprocedural control flow graphs
//! for an example of an edge that is only meant for information flow and not actual control flow.
//!
//! # General assumptions
//!
//! The graph construction algorithm assumes
//! that each basic block of the program term ends with zero, one or two jump instructions.
//! In the case of two jump instructions the first one is a conditional jump
//! and the second one is an unconditional jump.
//! Conditional calls are not supported.
//! Missing jump instructions are supported to indicate incomplete information about the control flow,
//! i.e. points where the control flow reconstruction failed.
//! These points are converted to dead ends in the control flow graphs.
//!
//! # Interprocedural control flow graph
//!
//! The function [`get_program_cfg`](fn.get_program_cfg.html) builds an interprocedural control flow graph out of a program term as follows:
//! * Each basic block is converted into two nodes, *BlkStart* and *BlkEnd*,
//! and a *block* edge from *BlkStart* to *BlkEnd*.
//! * Jumps and calls inside the program are converted to *Jump* or *Call* edges from the *BlkEnd* node of their source
//! to the *BlkStart* node of their target (which is the first block of the target function in case of calls).
//! * Calls to library functions outside the program are converted to *ExternCallStub* edges
//! from the *BlkEnd* node of the callsite to the *BlkStart* node of the basic block the call returns to
//! (if the call returns at all).
//! * For each in-program call and corresponding return jump one node and three edges are generated:
//!   * An artificial node *CallReturn*
//!   * A *CRCallStub* edge from the *BlkEnd* node of the callsite to *CallReturn*
//!   * A *CRReturnStub* edge from the *BlkEnd* node of the returning from block to *CallReturn*
//!   * A *CRCombine* edge from *CallReturn* to the *BlkStart* node of the returned to block.
//!
//! The artificial *CallReturn* nodes enable enriching the information flowing through a return edge
//! with information recovered from the corresponding callsite during a fixpoint computation.

use crate::prelude::*;
use crate::term::*;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::{HashMap, HashSet};

/// The graph type of an interprocedural control flow graph
pub type Graph<'a> = DiGraph<Node<'a>, Edge<'a>>;

/// The node type of an interprocedural control flow graph
///
/// Each node carries a pointer to its associated block with it.
/// For `CallReturn`nodes the associated block is the callsite block (containing the call instruction)
/// and *not* the return block (containing the return instruction).
#[derive(Serialize, Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum Node<'a> {
    BlkStart(&'a Term<Blk>),
    BlkEnd(&'a Term<Blk>),
    CallReturn(&'a Term<Blk>),
}

impl<'a> Node<'a> {
    /// Get the block corresponding to the node.
    pub fn get_block(&self) -> &'a Term<Blk> {
        use Node::*;
        match self {
            BlkStart(blk) | BlkEnd(blk) | CallReturn(blk) => blk,
        }
    }
}

impl<'a> std::fmt::Display for Node<'a> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::BlkStart(block) => write!(formatter, "BlkStart @ {}", block.tid),
            Self::BlkEnd(block) => write!(formatter, "BlkEnd @ {}", block.tid),
            Self::CallReturn(block) => write!(formatter, "CallReturn (caller @ {})", block.tid),
        }
    }
}

/// The edge type of an interprocedural fixpoint graph.
///
/// Where applicable the edge carries a reference to the corresponding jump instruction.
/// For `CRCombine` edges the corresponding jump is the call and not the return jump.
/// Intraprocedural jumps carry a second optional reference,
/// which is only set if the jump directly follows an conditional jump,
/// i.e. it represents the "conditional jump not taken" branch.
/// In this case the other jump reference points to the untaken conditional jump.
#[derive(Serialize, Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum Edge<'a> {
    Block,
    Jump(&'a Term<Jmp>, Option<&'a Term<Jmp>>),
    Call(&'a Term<Jmp>),
    ExternCallStub(&'a Term<Jmp>),
    CRCallStub,
    CRReturnStub,
    CRCombine(&'a Term<Jmp>),
}

/// A builder struct for building graphs
struct GraphBuilder<'a> {
    program: &'a Term<Program>,
    extern_subs: HashSet<Tid>,
    graph: Graph<'a>,
    /// Denotes the NodeIndices of possible jump targets
    jump_targets: HashMap<Tid, (NodeIndex, NodeIndex)>,
    /// for each function the list of return addresses of the corresponding call sites
    return_addresses: HashMap<Tid, Vec<(NodeIndex, NodeIndex)>>,
}

impl<'a> GraphBuilder<'a> {
    /// create a new builder with an emtpy graph
    pub fn new(program: &'a Term<Program>, extern_subs: HashSet<Tid>) -> GraphBuilder<'a> {
        GraphBuilder {
            program,
            extern_subs,
            graph: Graph::new(),
            jump_targets: HashMap::new(),
            return_addresses: HashMap::new(),
        }
    }

    /// add start and end nodes of a block and the connecting edge
    fn add_block(&mut self, block: &'a Term<Blk>) {
        let start = self.graph.add_node(Node::BlkStart(block));
        let end = self.graph.add_node(Node::BlkEnd(block));
        self.jump_targets.insert(block.tid.clone(), (start, end));
        self.graph.add_edge(start, end, Edge::Block);
    }

    /// add all blocks of the program to the graph
    fn add_program_blocks(&mut self) {
        let subs = self.program.term.subs.iter();
        let blocks = subs.map(|sub| sub.term.blocks.iter()).flatten();
        for block in blocks {
            self.add_block(block);
        }
    }

    /// add all subs to the jump targets so that call instructions can be linked to the starting block of the corresponding sub.
    fn add_subs_to_jump_targets(&mut self) {
        for sub in self.program.term.subs.iter() {
            if !sub.term.blocks.is_empty() {
                let start_block = &sub.term.blocks[0];
                let target_index = self.jump_targets[&start_block.tid];
                self.jump_targets.insert(sub.tid.clone(), target_index);
            }
            // TODO: Generate Log-Message for Subs without blocks.
        }
    }

    /// add call edges and interprocedural jump edges for a specific jump term to the graph
    fn add_jump_edge(
        &mut self,
        source: NodeIndex,
        jump: &'a Term<Jmp>,
        untaken_conditional: Option<&'a Term<Jmp>>,
    ) {
        match &jump.term.kind {
            JmpKind::Goto(Label::Direct(tid)) => {
                self.graph.add_edge(
                    source,
                    self.jump_targets[&tid].0,
                    Edge::Jump(jump, untaken_conditional),
                );
            }
            JmpKind::Goto(Label::Indirect(_)) => (), // TODO: add handling of indirect edges!
            JmpKind::Call(ref call) => {
                if let Label::Direct(ref target_tid) = call.target {
                    if self.extern_subs.contains(target_tid) {
                        if let Some(Label::Direct(ref return_tid)) = call.return_ {
                            self.graph.add_edge(
                                source,
                                self.jump_targets[&return_tid].0,
                                Edge::ExternCallStub(jump),
                            );
                        }
                    } else {
                        if let Some(target) = self.jump_targets.get(&target_tid) {
                            self.graph.add_edge(source, target.0, Edge::Call(jump));
                        }

                        if let Some(Label::Direct(ref return_tid)) = call.return_ {
                            let return_index = self.jump_targets[return_tid].0;
                            self.return_addresses
                                .entry(target_tid.clone())
                                .and_modify(|vec| vec.push((source, return_index)))
                                .or_insert_with(|| vec![(source, return_index)]);
                        }
                        // TODO: Non-returning calls and tail calls both have no return target in BAP.
                        // Thus we need to distinguish them somehow to correctly handle tail calls.
                    }
                }
            }
            JmpKind::Interrupt {
                value: _,
                return_addr: _,
            } => (), // TODO: Add some handling for interrupts
            JmpKind::Return(_) => {} // return edges are handled in a different function
        }
    }

    /// Add all outgoing edges generated by calls and interprocedural jumps for a specific block to the graph.
    /// Return edges are *not* added by this function.
    fn add_outgoing_edges(&mut self, node: NodeIndex) {
        let block: &'a Term<Blk> = self.graph[node].get_block();
        let jumps = block.term.jmps.as_slice();
        match jumps {
            [] => (), // Blocks without jumps are dead ends corresponding to control flow reconstruction errors.
            [jump] => self.add_jump_edge(node, jump, None),
            [if_jump, else_jump] => {
                self.add_jump_edge(node, if_jump, None);
                self.add_jump_edge(node, else_jump, Some(if_jump));
            }
            _ => panic!("Basic block with more than 2 jumps encountered"),
        }
    }

    /// For each return instruction and each corresponding call, add the following to the graph:
    /// - a CallReturn node.
    /// - edges from the callsite and from the returning-from site to the CallReturn node
    /// - an edge from the CallReturn node to the return-to site
    fn add_call_return_node_and_edges(
        &mut self,
        return_from_sub: &Term<Sub>,
        return_source: NodeIndex,
    ) {
        if self.return_addresses.get(&return_from_sub.tid).is_none() {
            return;
        }
        for (call_node, return_to_node) in self.return_addresses[&return_from_sub.tid].iter() {
            let call_block = self.graph[*call_node].get_block();
            let call_term = call_block
                .term
                .jmps
                .iter()
                .find(|jump| matches!(jump.term.kind, JmpKind::Call(_)))
                .unwrap();
            let cr_combine_node = self.graph.add_node(Node::CallReturn(call_block));
            self.graph
                .add_edge(*call_node, cr_combine_node, Edge::CRCallStub);
            self.graph
                .add_edge(return_source, cr_combine_node, Edge::CRReturnStub);
            self.graph
                .add_edge(cr_combine_node, *return_to_node, Edge::CRCombine(call_term));
        }
    }

    /// Add all return instruction related edges and nodes to the graph (for all return instructions).
    fn add_return_edges(&mut self) {
        for sub in &self.program.term.subs {
            for block in &sub.term.blocks {
                if block
                    .term
                    .jmps
                    .iter()
                    .any(|jmp| matches!(jmp.term.kind, JmpKind::Return(_)))
                {
                    let return_from_node = self.jump_targets[&block.tid].1;
                    self.add_call_return_node_and_edges(sub, return_from_node);
                }
            }
        }
    }

    /// Add all non-return-instruction-related jump edges to the graph.
    fn add_jump_and_call_edges(&mut self) {
        for node in self.graph.node_indices() {
            if let Node::BlkEnd(_) = self.graph[node] {
                self.add_outgoing_edges(node);
            }
        }
    }

    /// Build the interprocedural control flow graph.
    pub fn build(mut self) -> Graph<'a> {
        self.add_program_blocks();
        self.add_subs_to_jump_targets();
        self.add_jump_and_call_edges();
        self.add_return_edges();
        self.graph
    }
}

/// Build the interprocedural control flow graph for a program term.
pub fn get_program_cfg(program: &Term<Program>, extern_subs: HashSet<Tid>) -> Graph {
    let builder = GraphBuilder::new(program, extern_subs);
    builder.build()
}

/// For a given set of block TIDs generate a map from the TIDs to the indices of the BlkStart and BlkEnd nodes
/// corresponding to the block.
pub fn get_indices_of_block_nodes<'a, I: Iterator<Item = &'a Tid>>(
    graph: &'a Graph,
    block_tids: I,
) -> HashMap<Tid, (NodeIndex, NodeIndex)> {
    let tids: HashSet<Tid> = block_tids.cloned().collect();
    let mut tid_to_indices_map = HashMap::new();
    for node_index in graph.node_indices() {
        if let Some(tid) = tids.get(&graph[node_index].get_block().tid) {
            if let Node::BlkStart(_block_term) = graph[node_index] {
                let start_index = node_index;
                let end_index = graph.neighbors(start_index).next().unwrap();
                tid_to_indices_map.insert(tid.clone(), (start_index, end_index));
            }
        }
    }
    tid_to_indices_map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_program() -> Term<Program> {
        use Label::*;
        let call = Call {
            target: Direct(Tid::new("sub2")),
            return_: Some(Direct(Tid::new("sub1_blk2"))),
        };
        let call_term = Term {
            tid: Tid::new("call".to_string()),
            term: Jmp {
                condition: None,
                kind: JmpKind::Call(call),
            },
        };
        let return_term = Term {
            tid: Tid::new("return".to_string()),
            term: Jmp {
                condition: None,
                kind: JmpKind::Return(Direct(Tid::new("sub1_blk2"))),
            },
        };
        let jmp = Jmp {
            condition: None,
            kind: JmpKind::Goto(Direct(Tid::new("sub1_blk1"))),
        };
        let jmp_term = Term {
            tid: Tid::new("jump"),
            term: jmp,
        };
        let sub1_blk1 = Term {
            tid: Tid::new("sub1_blk1"),
            term: Blk {
                defs: Vec::new(),
                jmps: vec![call_term],
            },
        };
        let sub1_blk2 = Term {
            tid: Tid::new("sub1_blk2"),
            term: Blk {
                defs: Vec::new(),
                jmps: vec![jmp_term],
            },
        };
        let sub1 = Term {
            tid: Tid::new("sub1"),
            term: Sub {
                name: "sub1".to_string(),
                blocks: vec![sub1_blk1, sub1_blk2],
            },
        };
        let sub2_blk1 = Term {
            tid: Tid::new("sub2_blk1"),
            term: Blk {
                defs: Vec::new(),
                jmps: vec![return_term],
            },
        };
        let sub2 = Term {
            tid: Tid::new("sub2"),
            term: Sub {
                name: "sub2".to_string(),
                blocks: vec![sub2_blk1],
            },
        };
        let program = Term {
            tid: Tid::new("program"),
            term: Program {
                subs: vec![sub1, sub2],
                extern_symbols: Vec::new(),
                entry_points: Vec::new(),
            },
        };
        program
    }

    #[test]
    fn create_program_cfg() {
        let program = mock_program();
        let graph = get_program_cfg(&program, HashSet::new());
        println!("{}", serde_json::to_string_pretty(&graph).unwrap());
        assert_eq!(graph.node_count(), 7);
        assert_eq!(graph.edge_count(), 8);
    }
}
