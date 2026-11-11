//! `unresolved-type-ref` across models: a variable's type prefixed with an
//! import's name resolves in the imported model, as the evaluator resolves it.
//! The fixture corpus checks one artifact at a time, so this lives here.

use rbpmn_dmn::check;

const EXPORTING: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<definitions xmlns="https://www.omg.org/spec/DMN/20191111/MODEL/"
             namespace="https://rbpmn.example/tiers" name="tiers" id="_tiers">
  <itemDefinition name="Tier" id="tier"><typeRef>string</typeRef></itemDefinition>
</definitions>"#;

fn importing(type_ref: &str) -> String {
    format!(
        r##"<?xml version="1.0" encoding="UTF-8"?>
<definitions xmlns="https://www.omg.org/spec/DMN/20191111/MODEL/"
             namespace="https://rbpmn.example/orders" name="orders" id="_orders">
  <import namespace="https://rbpmn.example/tiers" name="tiers" importType="https://www.omg.org/spec/DMN/20191111/MODEL/"/>
  <inputData name="Level" id="level"><variable name="Level" typeRef="{type_ref}"/></inputData>
  <decision name="IsGold" id="isGold">
    <variable name="IsGold" typeRef="boolean"/>
    <informationRequirement><requiredInput href="#level"/></informationRequirement>
    <literalExpression><text>Level = "gold"</text></literalExpression>
  </decision>
</definitions>"##
    )
}

fn rules(type_ref: &str) -> Vec<(String, String)> {
    check(&[EXPORTING.to_string(), importing(type_ref)])
        .diagnostics
        .into_iter()
        .map(|d| (d.rule, d.element))
        .collect()
}

#[test]
fn an_imported_type_resolves_through_its_prefix() {
    assert_eq!(rules("tiers.Tier"), vec![]);
}

#[test]
fn an_imported_name_without_the_prefix_does_not() {
    assert_eq!(
        rules("Tier"),
        vec![("unresolved-type-ref".to_string(), "level".to_string())]
    );
}

#[test]
fn a_prefix_naming_a_type_the_import_lacks_does_not() {
    assert_eq!(
        rules("tiers.Rank"),
        vec![("unresolved-type-ref".to_string(), "level".to_string())]
    );
}
