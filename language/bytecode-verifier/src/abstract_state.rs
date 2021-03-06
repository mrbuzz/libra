// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module defines the abstract state for the type and memory safety analysis.
use crate::{
    absint::{AbstractDomain, JoinResult},
    borrow_graph::BorrowGraph,
    ref_id::RefID,
};
use mirai_annotations::{checked_postcondition, checked_precondition, checked_verify};
use std::collections::{BTreeMap, BTreeSet};
use vm::{
    file_format::{
        CompiledModule, FieldDefinitionIndex, Kind, LocalIndex, SignatureToken,
        StructDefinitionIndex,
    },
    views::{FunctionDefinitionView, ViewInternals},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypedAbstractValue {
    pub signature: SignatureToken,
    pub value: AbstractValue,
}

/// AbstractValue represents a value either on the evaluation stack or
/// in a local on a frame of the function stack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbstractValue {
    Reference(RefID),
    Value(Kind),
}

impl AbstractValue {
    /// checks if self is a reference
    pub fn is_reference(&self) -> bool {
        match self {
            AbstractValue::Reference(_) => true,
            AbstractValue::Value(_) => false,
        }
    }

    /// checks if self is a value
    pub fn is_value(&self) -> bool {
        !self.is_reference()
    }

    /// checks if self is a non-resource value
    pub fn is_unrestricted_value(&self) -> bool {
        match self {
            AbstractValue::Reference(_) => false,
            AbstractValue::Value(Kind::Unrestricted) => true,
            AbstractValue::Value(Kind::All) | AbstractValue::Value(Kind::Resource) => false,
        }
    }

    /// checks if self is a resource or all value
    pub fn is_possibly_resource(&self) -> bool {
        match self {
            AbstractValue::Reference(_) => false,
            AbstractValue::Value(Kind::Unrestricted) => false,
            AbstractValue::Value(Kind::All) | AbstractValue::Value(Kind::Resource) => true,
        }
    }

    /// possibly extracts id from self
    pub fn extract_id(&self) -> Option<RefID> {
        match self {
            AbstractValue::Reference(id) => Some(*id),
            AbstractValue::Value(_) => None,
        }
    }
}

/// LabelElem is an element of a label on an edge in the borrow graph.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LabelElem {
    Local(LocalIndex),
    Global(StructDefinitionIndex),
    Field(FieldDefinitionIndex),
}

impl Default for LabelElem {
    fn default() -> Self {
        LabelElem::Local(0)
    }
}

/// AbstractState is the analysis state over which abstract interpretation is performed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AbstractState {
    locals: BTreeMap<LocalIndex, TypedAbstractValue>,
    borrow_graph: BorrowGraph<LabelElem>,
    num_locals: usize,
    next_id: usize,
}

impl AbstractState {
    /// create a new abstract state
    pub fn new(function_definition_view: FunctionDefinitionView<CompiledModule>) -> Self {
        let function_signature_view = function_definition_view.signature();
        let mut locals = BTreeMap::new();
        let mut borrow_graph = BorrowGraph::new();
        for (arg_idx, arg_type_view) in function_signature_view.arg_tokens().enumerate() {
            if arg_type_view.is_reference() {
                let id = RefID::new(arg_idx);
                borrow_graph.add(id);
                locals.insert(
                    arg_idx as LocalIndex,
                    TypedAbstractValue {
                        signature: arg_type_view.as_inner().clone(),
                        value: AbstractValue::Reference(id),
                    },
                );
            } else {
                let arg_kind = arg_type_view
                    .kind(&function_definition_view.signature().as_inner().type_formals);
                locals.insert(
                    arg_idx as LocalIndex,
                    TypedAbstractValue {
                        signature: arg_type_view.as_inner().clone(),
                        value: AbstractValue::Value(arg_kind),
                    },
                );
            }
        }
        let num_locals = function_definition_view.locals_signature().len();
        // ids in [0, num_locals] are reserved for constructing canonical state
        let next_id = num_locals + 1;
        let mut new_state = AbstractState {
            locals,
            borrow_graph,
            num_locals,
            next_id,
        };
        new_state.borrow_graph.add(new_state.frame_root());
        new_state
    }

    /// checks if local@idx is available
    pub fn is_available(&self, idx: LocalIndex) -> bool {
        self.locals.contains_key(&idx)
    }

    /// returns local@idx
    pub fn local(&self, idx: LocalIndex) -> &TypedAbstractValue {
        &self.locals[&idx]
    }

    /// removes local@idx
    pub fn remove_local(&mut self, idx: LocalIndex) -> TypedAbstractValue {
        self.locals.remove(&idx).unwrap()
    }

    /// inserts local@idx
    pub fn insert_local(&mut self, idx: LocalIndex, abs_type: TypedAbstractValue) {
        self.locals.insert(idx, abs_type);
    }

    /// checks if local@idx may be safely destroyed
    pub fn is_local_safe_to_destroy(&self, idx: LocalIndex) -> bool {
        match self.locals[&idx].value {
            AbstractValue::Reference(_) => true,
            AbstractValue::Value(Kind::All) | AbstractValue::Value(Kind::Resource) => false,
            AbstractValue::Value(Kind::Unrestricted) => !self.is_local_borrowed(idx),
        }
    }

    /// checks if the stack frame of the function being analyzed can be safely destroyed.
    /// safe destruction requires that all references in locals have already been destroyed
    /// and all values in locals are unrestricted and unborrowed.
    pub fn is_frame_safe_to_destroy(&self) -> bool {
        self.locals
            .values()
            .all(|x| x.value.is_unrestricted_value())
            && !self.is_borrowed(self.frame_root())
    }

    /// destroys local@idx
    pub fn destroy_local(&mut self, idx: LocalIndex) {
        checked_precondition!(self.is_local_safe_to_destroy(idx));
        let local = self.locals.remove(&idx).unwrap();
        match local.value {
            AbstractValue::Reference(id) => self.remove(id),
            AbstractValue::Value(kind) => {
                checked_verify!(kind == Kind::Unrestricted);
            }
        }
    }

    /// returns the frame root id
    fn frame_root(&self) -> RefID {
        RefID::new(self.num_locals)
    }

    /// adds and returns new id to borrow graph
    pub fn add(&mut self) -> RefID {
        let id = RefID::new(self.next_id);
        self.borrow_graph.add(id);
        self.next_id += 1;
        id
    }

    /// removes `id` from borrow graph
    pub fn remove(&mut self, id: RefID) {
        self.borrow_graph.remove(id);
    }

    /// checks if `id` is borrowed
    pub fn is_borrowed(&self, id: RefID) -> bool {
        !self.borrow_graph.all_borrows(id).is_empty()
    }

    fn local_borrows(&self, idx: LocalIndex) -> BTreeSet<RefID> {
        self.borrow_graph
            .consistent_borrows(self.frame_root(), LabelElem::Local(idx))
    }

    /// checks if local@idx is borrowed
    pub fn is_local_borrowed(&self, idx: LocalIndex) -> bool {
        !self.local_borrows(idx).is_empty()
    }

    /// checks if local@idx is mutably borrowed
    pub fn is_local_mutably_borrowed(&self, idx: LocalIndex) -> bool {
        !self.all_immutable(self.local_borrows(idx))
    }

    /// checks if global@idx is borrowed
    pub fn is_global_borrowed(&self, idx: StructDefinitionIndex) -> bool {
        !self
            .borrow_graph
            .consistent_borrows(self.frame_root(), LabelElem::Global(idx))
            .is_empty()
    }

    /// checks if `id` is freezable
    pub fn is_freezable(&self, id: RefID) -> bool {
        let borrows = self.borrow_graph.all_borrows(id);
        self.all_immutable(borrows)
    }

    /// update self to reflect a borrow of global@idx by a fresh id that is returned
    pub fn borrow_global_value(&mut self, mut_: bool, idx: StructDefinitionIndex) -> Option<RefID> {
        if mut_ {
            if self.is_global_borrowed(idx) {
                return None;
            }
        } else {
            let borrowed_ids = self
                .borrow_graph
                .consistent_borrows(self.frame_root(), LabelElem::Global(idx));
            if !self.all_immutable(borrowed_ids) {
                return None;
            }
        }
        let new_id = self.add();
        self.borrow_graph
            .add_weak_edge(self.frame_root(), vec![LabelElem::Global(idx)], new_id);
        Some(new_id)
    }

    /// update self to reflect a borrow of field@idx from operand.value by a fresh id that is returned
    pub fn borrow_field(
        &mut self,
        operand: &TypedAbstractValue,
        mut_: bool,
        idx: FieldDefinitionIndex,
    ) -> Option<RefID> {
        let id = operand.value.extract_id().unwrap();
        if mut_ {
            if !self.borrow_graph.nil_borrows(id).is_empty() {
                return None;
            }
        } else if operand.signature.is_mutable_reference() {
            let borrowed_ids = self
                .borrow_graph
                .consistent_borrows(id, LabelElem::Field(idx));
            if !self.all_immutable(borrowed_ids) {
                return None;
            }
        }
        let new_id = self.add();
        self.borrow_graph
            .add_strong_edge(id, vec![LabelElem::Field(idx)], new_id);
        Some(new_id)
    }

    /// update self to reflect a borrow of local@idx (which must be a value) by a fresh id that is returned
    pub fn borrow_local_value(&mut self, mut_: bool, idx: LocalIndex) -> Option<RefID> {
        checked_precondition!(self.locals[&idx].value.is_value());
        if !mut_ {
            // nothing to check in case borrow is mutable since the frame cannot have a NIL outgoing edge
            let borrowed_ids = self
                .borrow_graph
                .consistent_borrows(self.frame_root(), LabelElem::Local(idx));
            if !self.all_immutable(borrowed_ids) {
                return None;
            }
        }
        let new_id = self.add();
        self.borrow_graph
            .add_strong_edge(self.frame_root(), vec![LabelElem::Local(idx)], new_id);
        Some(new_id)
    }

    /// update self to reflect a borrow of local@idx (which must be a reference) by a fresh id that is returned
    pub fn borrow_local_reference(&mut self, idx: LocalIndex) -> RefID {
        checked_precondition!(self.locals[&idx].value.is_reference());
        let new_id = self.add();
        self.borrow_graph.add_strong_edge(
            self.locals[&idx].value.extract_id().unwrap(),
            vec![],
            new_id,
        );
        new_id
    }

    /// update self to reflect a borrow from each id in to_borrow_from by a fresh id that is returned
    pub fn borrow_from(&mut self, to_borrow_from: &BTreeSet<RefID>) -> RefID {
        let new_id = self.add();
        for id in to_borrow_from {
            self.borrow_graph.add_weak_edge(*id, vec![], new_id);
        }
        new_id
    }

    /// returns the canonical representation of self
    pub fn construct_canonical_state(&self) -> Self {
        let mut id_map = BTreeMap::new();
        id_map.insert(self.frame_root(), self.frame_root());
        let locals = self
            .locals
            .iter()
            .map(|(idx, abs)| {
                let new_abs = match &abs.value {
                    AbstractValue::Reference(id) => {
                        let new_id = RefID::new(*idx as usize);
                        id_map.insert(*id, new_id);
                        TypedAbstractValue {
                            signature: abs.signature.clone(),
                            value: AbstractValue::Reference(new_id),
                        }
                    }
                    _ => abs.clone(),
                };
                (*idx, new_abs)
            })
            .collect::<BTreeMap<_, _>>();
        checked_verify!(self.locals.len() == locals.len());
        let canonical_state = AbstractState {
            locals,
            borrow_graph: self.borrow_graph.rename_ids(id_map),
            num_locals: self.num_locals,
            next_id: self.num_locals + 1,
        };
        checked_postcondition!(canonical_state.is_canonical());
        canonical_state
    }

    fn all_immutable(&self, borrows: BTreeSet<RefID>) -> bool {
        !self.locals.values().any(|abs_type| {
            abs_type.signature.is_mutable_reference()
                && borrows.contains(&abs_type.value.extract_id().unwrap())
        })
    }

    fn is_canonical(&self) -> bool {
        self.num_locals + 1 == self.next_id
            && self.locals.iter().all(|(x, y)| {
                !y.value.is_reference() || RefID::new(*x as usize) == y.value.extract_id().unwrap()
            })
    }

    fn iter_locals(&self) -> impl Iterator<Item = LocalIndex> {
        0..self.num_locals as LocalIndex
    }

    /// returns `Some` of the self joined with other,
    /// returns `None` if there is a join error
    pub fn join_(&self, other: &Self) -> Option<Self> {
        checked_precondition!(self.is_canonical() && other.is_canonical());
        checked_precondition!(self.next_id == other.next_id);
        checked_precondition!(self.num_locals == other.num_locals);
        let mut locals = BTreeMap::new();
        let mut self_graph = self.borrow_graph.clone();
        let mut other_graph = other.borrow_graph.clone();
        for idx in self.iter_locals() {
            let self_value = self.locals.get(&idx);
            let other_value = other.locals.get(&idx);
            match (self_value, other_value) {
                // Unavailable on both sides, nothing to add
                (None, None) => (),

                // Join error, a resource is available along one path but not the other
                (None, Some(v)) | (Some(v), None) if v.value.is_possibly_resource() => return None,

                // The local has a unrestricted value on one side but not the other, nothing to add
                (Some(v), None) => {
                    // A reference exists on one side, but not the other. Release
                    if let AbstractValue::Reference(id) = &v.value {
                        self_graph.remove(*id);
                    }
                }
                (None, Some(v)) => {
                    // A reference exists on one side, but not the other. Release
                    if let AbstractValue::Reference(id) = &v.value {
                        other_graph.remove(*id);
                    }
                }

                // The local has a value on each side, add it to the state
                (Some(v1), Some(v2)) => {
                    checked_verify!(v1 == v2);
                    checked_verify!(!locals.contains_key(&idx));
                    locals.insert(idx, v1.clone());
                }
            }
        }

        let borrow_graph = {
            self_graph.join(&other_graph);
            self_graph
        };
        let next_id = self.next_id;
        let num_locals = self.num_locals;

        Some(Self {
            locals,
            borrow_graph,
            next_id,
            num_locals,
        })
    }
}

impl AbstractDomain for AbstractState {
    /// attempts to join state to self and returns the result
    fn join(&mut self, state: &AbstractState) -> JoinResult {
        let joined = match Self::join_(self, state) {
            None => return JoinResult::Error,
            Some(joined) => joined,
        };
        checked_verify!(self.num_locals == joined.num_locals);
        let locals_unchanged = self
            .iter_locals()
            .all(|idx| self.locals.get(&idx) == joined.locals.get(&idx));
        let borrow_graph_unchanged = self.borrow_graph.abstracts(&joined.borrow_graph);
        if locals_unchanged && borrow_graph_unchanged {
            JoinResult::Unchanged
        } else {
            *self = joined;
            JoinResult::Changed
        }
    }
}
