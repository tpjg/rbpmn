//! Every place a FEEL expression can hide in a DMN model.
//!
//! `ModelEvaluator::new` already refuses a model whose FEEL does not parse,
//! so this walk is not there to find syntax errors — it is there so that
//! `feel-deterministic` can be told *where* an expression lives, and so that
//! a banned builtin cannot survive by sitting in a corner of the model nobody
//! looked at.
//!
//! **The matches below are exhaustive on purpose.** Not one of them ends in a
//! `_ =>` arm, so a dsntk upgrade that adds an expression kind breaks this
//! build instead of silently widening what rbpmn accepts. That is the whole
//! reason to walk the typed model rather than the XML: the compiler is the
//! only reviewer guaranteed to notice.

use dsntk_model::*;

/// Which FEEL grammar a slot is written in. DMN uses two, and they are not
/// interchangeable: `< 100` is a valid unary test and not a valid
/// expression, so parsing an input entry as an expression would report a
/// syntax error in a perfectly good decision table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grammar {
    Expression,
    UnaryTests,
}

/// One FEEL expression, and enough about where it came from to point a
/// modeler at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeelExpression {
    /// The DRG element it belongs to — the `element` of any diagnostic
    /// raised about it.
    pub element: String,
    /// What kind of slot it sits in, for the message: "literal expression",
    /// "input entry", "allowed values", ...
    pub slot: &'static str,
    /// The FEEL source.
    pub text: String,
    /// Which grammar to parse `text` with.
    pub grammar: Grammar,
}

/// A locator for diagnostics: the element's id when the model gave it one,
/// its name otherwise. DMN ids are optional (dsntk synthesises one when the
/// document omits it), names are not — and a synthesised id means nothing to
/// the person reading the diagnostic.
fn locate<T: DmnElement + NamedElement>(element: &T) -> String {
    match element.opt_id() {
        Some(id) if !id.is_empty() => id.clone(),
        _ => element.name().to_string(),
    }
}

/// A function whose body is not FEEL at all — DMN lets a boxed function
/// delegate to Java or PMML, and dsntk's Java binding POSTs to a JVM on
/// localhost. Found here because it is a property of the *model*, not of any
/// expression text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalFunction {
    pub element: String,
    /// "Java" or "PMML".
    pub kind: &'static str,
}

/// Everything in an artifact that the deploy-time rules need to look at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub expressions: Vec<FeelExpression>,
    pub externals: Vec<ExternalFunction>,
}

/// Collect every FEEL expression in the artifact, and every external function.
pub fn collect(definitions: &Definitions) -> Found {
    let mut out = Collector {
        out: Found::default(),
    };
    for item in definitions.item_definitions() {
        out.item_definition(&locate(item), item);
    }
    for element in definitions.drg_elements() {
        match element {
            DrgElement::Decision(d) => {
                let at = locate(d);
                if let Some(logic) = d.decision_logic() {
                    out.expression(&at, logic);
                }
            }
            DrgElement::BusinessKnowledgeModel(bkm) => {
                let at = locate(bkm);
                if let Some(function) = bkm.encapsulated_logic() {
                    out.function_definition(&at, function);
                }
            }
            // No FEEL: input data and knowledge sources carry a variable and
            // references, and a decision service composes other elements.
            DrgElement::InputData(_)
            | DrgElement::KnowledgeSource(_)
            | DrgElement::DecisionService(_) => {}
        }
    }
    out.out
}

struct Collector {
    out: Found,
}

impl Collector {
    fn push(&mut self, element: &str, slot: &'static str, grammar: Grammar, text: &str) {
        // Empty slots are legal DMN (an unfilled cell), and an empty string is
        // not an expression to check. A `-` is DMN's "any value" in a
        // decision table and parses as a unary test, so it is left alone.
        if !text.trim().is_empty() {
            self.out.expressions.push(FeelExpression {
                element: element.to_string(),
                slot,
                text: text.to_string(),
                grammar,
            });
        }
    }

    fn maybe(
        &mut self,
        element: &str,
        slot: &'static str,
        grammar: Grammar,
        text: &Option<String>,
    ) {
        if let Some(text) = text {
            self.push(element, slot, grammar, text);
        }
    }

    /// Item definitions constrain values with unary tests, and nest.
    fn item_definition(&mut self, element: &str, item: &ItemDefinition) {
        if let Some(allowed) = item.allowed_values() {
            self.maybe(
                element,
                "allowed values",
                Grammar::UnaryTests,
                allowed.text(),
            );
        }
        for component in item.item_components() {
            self.item_definition(element, component);
        }
    }

    fn expression(&mut self, element: &str, expression: &ExpressionInstance) {
        match expression {
            ExpressionInstance::Conditional(e) => {
                self.child(element, e.if_expression());
                self.child(element, e.then_expression());
                self.child(element, e.else_expression());
            }
            ExpressionInstance::Context(e) => {
                for entry in e.context_entries() {
                    self.expression(element, &entry.value);
                }
            }
            ExpressionInstance::DecisionTable(e) => self.decision_table(element, e),
            ExpressionInstance::Every(e) => {
                self.typed_child(element, e.in_expression());
                self.child(element, e.satisfies_expression());
            }
            ExpressionInstance::Filter(e) => {
                self.child(element, e.in_expression());
                self.child(element, e.match_expression());
            }
            ExpressionInstance::For(e) => {
                self.typed_child(element, e.in_expression());
                self.child(element, e.return_expression());
            }
            ExpressionInstance::FunctionDefinition(e) => self.function_definition(element, e),
            ExpressionInstance::Invocation(e) => {
                for binding in e.bindings() {
                    if let Some(bound) = binding.binding_formula() {
                        self.expression(element, bound);
                    }
                }
                self.expression(element, e.called_function());
            }
            ExpressionInstance::List(e) => {
                for item in e.elements() {
                    self.expression(element, item);
                }
            }
            ExpressionInstance::LiteralExpression(e) => {
                self.maybe(element, "literal expression", Grammar::Expression, e.text());
            }
            ExpressionInstance::Relation(e) => {
                for row in e.rows() {
                    for cell in row.elements() {
                        self.expression(element, cell);
                    }
                }
            }
            ExpressionInstance::Some(e) => {
                self.typed_child(element, e.in_expression());
                self.child(element, e.satisfies_expression());
            }
        }
    }

    fn child(&mut self, element: &str, child: &ChildExpression) {
        self.expression(element, child.value());
    }

    fn typed_child(&mut self, element: &str, child: &TypedChildExpression) {
        self.expression(element, child.value());
    }

    fn function_definition(&mut self, element: &str, function: &FunctionDefinition) {
        // Exhaustive on purpose: a new function kind must be classified, not
        // inherited as "probably fine".
        match function.kind() {
            FunctionKind::Feel => {}
            FunctionKind::Java => self.external(element, "Java"),
            FunctionKind::Pmml => self.external(element, "PMML"),
        }
        if let Some(body) = function.body() {
            self.expression(element, body);
        }
    }

    fn external(&mut self, element: &str, kind: &'static str) {
        self.out.externals.push(ExternalFunction {
            element: element.to_string(),
            kind,
        });
    }

    /// A decision table hides FEEL in six different slots, and every one of
    /// them is evaluated: input expressions, the unary tests that constrain
    /// them, each rule's input and output entries, and the default output
    /// taken when nothing matches.
    fn decision_table(&mut self, element: &str, table: &DecisionTable) {
        for input in table.input_clauses() {
            self.push(
                element,
                "input expression",
                Grammar::Expression,
                &input.input_expression,
            );
            self.maybe(
                element,
                "allowed input values",
                Grammar::UnaryTests,
                &input.allowed_input_values,
            );
        }
        for output in table.output_clauses() {
            self.maybe(
                element,
                "allowed output values",
                Grammar::UnaryTests,
                &output.allowed_output_values,
            );
            self.maybe(
                element,
                "default output entry",
                Grammar::Expression,
                &output.default_output_entry,
            );
        }
        for rule in table.rules() {
            for entry in &rule.input_entries {
                self.push(element, "input entry", Grammar::UnaryTests, &entry.text);
            }
            for entry in &rule.output_entries {
                self.push(element, "output entry", Grammar::Expression, &entry.text);
            }
        }
    }
}
