//! Elixir highlights in time linear in how deeply its syntax tree nests.
//!
//! An operator chain parses as a left-nested `binary_operator` per operator,
//! and a run of nested calls as a `call` per level, so a few hundred terms on
//! one line are a few hundred levels of tree. Generated sources reach that
//! depth: guards that list every Unicode range one `or` at a time. Each shape
//! here renders in milliseconds and took seconds, growing with the square of
//! the depth, while the locals query held patterns open across every level.

use lumis::{languages::Language, HighlightOptions, HtmlLinkedBuilder};

const DEPTH: usize = 500;

fn render(body: &str) -> String {
    let source = format!("defmodule M do\n  def f(x) do\n    {body}\n  end\nend\n");
    let formatter = HtmlLinkedBuilder::new()
        .language(Language::Elixir)
        .build()
        .unwrap();

    lumis::highlight_with_options(&source, formatter, HighlightOptions::new())
}

fn assert_highlighted_within_budget(html: &str) {
    assert!(
        !html.contains("data-lumis-budget"),
        "the render ran out of its default budget, got {}",
        &html[..html.len().min(200)]
    );
    assert!(
        html.matches("<span").count() > DEPTH,
        "the document came back unhighlighted"
    );
}

#[test]
fn an_operator_chain_highlights_within_the_default_budget() {
    let body = (1..=DEPTH).fold("x === 0".to_string(), |chain, i| {
        format!("{chain} or x === {i}")
    });

    assert_highlighted_within_budget(&render(&body));
}

#[test]
fn a_pipe_chain_highlights_within_the_default_budget() {
    let body = (1..=DEPTH).fold("x".to_string(), |chain, i| format!("{chain} |> f{i}()"));

    assert_highlighted_within_budget(&render(&body));
}

#[test]
fn nested_calls_highlight_within_the_default_budget() {
    let body = format!("{}x{}", "f(".repeat(DEPTH), ")".repeat(DEPTH));

    assert_highlighted_within_budget(&render(&body));
}
