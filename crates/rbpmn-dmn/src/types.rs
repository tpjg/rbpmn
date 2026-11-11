//! Every place a DMN model names a type.
//!
//! `unresolved-type-ref` asks dsntk whether each of these resolves, because a
//! type it cannot resolve mostly does not stop the build. What happens instead
//! depends on the slot, measured against the evaluator: an input data value of
//! that type — directly, or through an item definition whose base type does not
//! resolve — evaluates to `null`; everywhere else the type is silently not
//! applied, so the value is neither checked nor converted. Both are a model that
//! means something other than what it says, which is what the rule refuses.
//! This walk only finds the names; which ones resolve is dsntk's answer
//! ([`dsntk_model_evaluator::ModelEvaluator::variable_type`] and
//! `information_item_type`), never a list kept here.
//!
//! Where an unresolved type stops the build — a function parameter, a context
//! entry, a function boxed in a context, an invocation binding with a formula —
//! the slot is walked too and never reaches the rule: the build refuses first,
//! as `dmn-validates`.
//!
//! Exhaustive like [`crate::expressions`], for the same reason: a dsntk upgrade
//! that adds an expression kind breaks this build instead of leaving a corner
//! of the model unchecked.
//!
//! A decision table's input-expression `typeRef` is not here because dsntk's
//! parser does not keep it — no evaluator ever applies it.

use crate::expressions::locate;
use dsntk_model::*;

/// One type name, and enough about where it came from to point a modeler at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRef {
    /// The DRG element or item definition it belongs to.
    pub element: String,
    /// What names the type, for the message: "variable", "output clause", ...
    pub slot: &'static str,
    /// The type name as written.
    pub name: String,
    /// How the evaluator resolves it, and what an unresolved one does.
    pub kind: Kind,
}

/// The three ways the evaluator treats a type name, measured per slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An input data variable: resolved as a variable (through the model's
    /// imports); unresolved, every value of the input evaluates to `null`.
    InputData,
    /// An item definition's base type or a component's: resolved as written,
    /// in the model's own namespace; unresolved, input data of this item
    /// definition evaluates to `null`, and elsewhere the type is not applied.
    ItemDefinition,
    /// Every other slot: resolved as a variable; unresolved, the type is not
    /// applied.
    NotApplied,
}

/// Collect every type name in the artifact.
pub fn collect(definitions: &Definitions) -> Vec<TypeRef> {
    let mut out = Collector { out: Vec::new() };
    for item in definitions.item_definitions() {
        out.item_definition(&locate(item), item);
    }
    for element in definitions.drg_elements() {
        match element {
            DrgElement::Decision(d) => {
                let at = locate(d);
                out.information_item(&at, "variable", d.variable());
                if let Some(logic) = d.decision_logic() {
                    out.expression(&at, logic);
                }
            }
            DrgElement::BusinessKnowledgeModel(bkm) => {
                let at = locate(bkm);
                out.information_item(&at, "variable", bkm.variable());
                if let Some(function) = bkm.encapsulated_logic() {
                    out.function_definition(&at, function);
                }
            }
            DrgElement::InputData(input) => {
                out.push(
                    &locate(input),
                    "variable",
                    input.variable().type_ref(),
                    Kind::InputData,
                );
            }
            DrgElement::DecisionService(service) => {
                out.information_item(&locate(service), "variable", service.variable());
            }
            // A knowledge source documents where knowledge comes from; it
            // carries no value and so no type.
            DrgElement::KnowledgeSource(_) => {}
        }
    }
    out.out
}

struct Collector {
    out: Vec<TypeRef>,
}

impl Collector {
    fn push(&mut self, element: &str, slot: &'static str, name: &str, kind: Kind) {
        self.out.push(TypeRef {
            element: element.to_string(),
            slot,
            name: name.to_string(),
            kind,
        });
    }

    fn maybe(&mut self, element: &str, slot: &'static str, name: &Option<String>) {
        if let Some(name) = name {
            self.push(element, slot, name, Kind::NotApplied);
        }
    }

    /// An information item always has a type: dsntk's parser writes `Any`
    /// where the document gave none.
    fn information_item(&mut self, element: &str, slot: &'static str, item: &InformationItem) {
        self.push(element, slot, item.type_ref(), Kind::NotApplied);
    }

    /// An item definition names its base type, or is a structure, a
    /// collection or a function signature built from others — and nests.
    fn item_definition(&mut self, element: &str, item: &ItemDefinition) {
        if let Some(name) = item.type_ref() {
            self.push(element, "item definition", name, Kind::ItemDefinition);
        }
        if let Some(function) = item.function_item() {
            self.maybe(element, "function output", function.output_type_ref());
            for parameter in function.parameters() {
                self.information_item(element, "function parameter", parameter);
            }
        }
        for component in item.item_components() {
            self.item_definition(element, component);
        }
    }

    fn expression(&mut self, element: &str, expression: &ExpressionInstance) {
        match expression {
            ExpressionInstance::Conditional(e) => {
                self.maybe(element, "conditional", e.type_ref());
                self.child(element, e.if_expression());
                self.child(element, e.then_expression());
                self.child(element, e.else_expression());
            }
            ExpressionInstance::Context(e) => {
                self.maybe(element, "context", e.type_ref());
                for entry in e.context_entries() {
                    if let Some(variable) = &entry.variable {
                        self.information_item(element, "context entry", variable);
                    }
                    self.expression(element, &entry.value);
                }
            }
            ExpressionInstance::DecisionTable(e) => {
                self.maybe(element, "decision table", e.type_ref());
                for output in e.output_clauses() {
                    self.maybe(element, "output clause", &output.type_ref);
                }
            }
            ExpressionInstance::Every(e) => {
                self.maybe(element, "every", e.type_ref());
                self.typed_child(element, e.in_expression());
                self.child(element, e.satisfies_expression());
            }
            ExpressionInstance::Filter(e) => {
                self.maybe(element, "filter", e.type_ref());
                self.child(element, e.in_expression());
                self.child(element, e.match_expression());
            }
            ExpressionInstance::For(e) => {
                self.maybe(element, "for", e.type_ref());
                self.typed_child(element, e.in_expression());
                self.child(element, e.return_expression());
            }
            ExpressionInstance::FunctionDefinition(e) => self.function_definition(element, e),
            ExpressionInstance::Invocation(e) => {
                self.maybe(element, "invocation", e.type_ref());
                for binding in e.bindings() {
                    self.information_item(element, "binding parameter", binding.parameter());
                    if let Some(bound) = binding.binding_formula() {
                        self.expression(element, bound);
                    }
                }
                self.expression(element, e.called_function());
            }
            ExpressionInstance::List(e) => {
                self.maybe(element, "list", e.type_ref());
                for item in e.elements() {
                    self.expression(element, item);
                }
            }
            ExpressionInstance::LiteralExpression(e) => {
                self.maybe(element, "literal expression", e.type_ref());
            }
            ExpressionInstance::Relation(e) => {
                self.maybe(element, "relation", e.type_ref());
                for column in e.columns() {
                    self.information_item(element, "relation column", column);
                }
                for row in e.rows() {
                    self.maybe(element, "relation row", row.type_ref());
                    for cell in row.elements() {
                        self.expression(element, cell);
                    }
                }
            }
            ExpressionInstance::Some(e) => {
                self.maybe(element, "some", e.type_ref());
                self.typed_child(element, e.in_expression());
                self.child(element, e.satisfies_expression());
            }
        }
    }

    fn child(&mut self, element: &str, child: &ChildExpression) {
        self.expression(element, child.value());
    }

    fn typed_child(&mut self, element: &str, child: &TypedChildExpression) {
        self.maybe(element, "iteration source", child.type_ref());
        self.expression(element, child.value());
    }

    fn function_definition(&mut self, element: &str, function: &FunctionDefinition) {
        self.maybe(element, "function definition", function.type_ref());
        for parameter in function.formal_parameters() {
            self.information_item(element, "function parameter", parameter);
        }
        if let Some(body) = function.body() {
            self.expression(element, body);
        }
    }
}
