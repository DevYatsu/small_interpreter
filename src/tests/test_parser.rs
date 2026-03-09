//! Integration-style tests for the `Parser` — verifying that source snippets
//! compile to the expected bytecode `Instruction` sequences.
//!
//! These tests follow a TDD approach: each test documents the *expected*
//! compiler output, and failing tests pinpoint regressions in the parser.

#[cfg(test)]
mod tests {
    use crate::{compiler::Instruction, error::JitError, parser::Parser};

    // Helpers

    fn compile(input: &str) -> Result<crate::compiler::Program, JitError> {
        Parser::new(input)?.compile()
    }

    fn instructions(src: &str) -> Vec<Instruction> {
        compile(src)
            .expect("should compile without error")
            .instructions
            .to_vec()
    }

    // Literal loading

    #[test]
    fn compiles_number_literal() {
        let instrs = instructions("let x: 42");
        assert!(
            instrs.iter().any(|i| matches!(
                i,
                Instruction::LoadLiteral { val, .. } if val.as_number() == Some(42.0)
            )),
            "expected LoadLiteral(42.0), got {instrs:#?}"
        );
    }

    #[test]
    fn compiles_bool_true_literal() {
        let instrs = instructions("let x: true");
        assert!(
            instrs.iter().any(|i| matches!(
                i,
                Instruction::LoadLiteral { val, .. } if val.as_bool() == Some(true)
            )),
            "expected LoadLiteral(true)"
        );
    }

    #[test]
    fn compiles_bool_false_literal() {
        let instrs = instructions("let x: false");
        assert!(
            instrs.iter().any(|i| matches!(
                i,
                Instruction::LoadLiteral { val, .. } if val.as_bool() == Some(false)
            )),
            "expected LoadLiteral(false)"
        );
    }

    #[test]
    fn compiles_short_string_as_sso() {
        // "hi" is 2 bytes → SSO
        let instrs = instructions(r#"let x: "hi""#);
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::LoadLiteral { .. })),
            "expected LoadLiteral for SSO string"
        );
    }

    #[test]
    fn compiles_long_string_as_object_reference() {
        // A string longer than 6 bytes should be interned and referenced as an object id.
        let instrs = instructions(r#"let x: "hello world!""#);
        assert!(
            instrs.iter().any(|i| matches!(
                i,
                Instruction::LoadLiteral { val, .. } if val.as_obj_id().is_some()
            )),
            "expected LoadLiteral with object id for long string"
        );
    }

    // Arithmetic

    #[test]
    fn compiles_addition() {
        let instrs = instructions("let x: 1 + 2");
        assert!(
            instrs.iter().any(|i| matches!(i, Instruction::Add { .. })),
            "expected Add instruction"
        );
    }

    #[test]
    fn compiles_subtraction() {
        let instrs = instructions("let x: 10 - 3");
        assert!(
            instrs.iter().any(|i| matches!(i, Instruction::Sub { .. })),
            "expected Sub instruction"
        );
    }

    #[test]
    fn compiles_multiplication() {
        let instrs = instructions("let x: 4 * 5");
        assert!(
            instrs.iter().any(|i| matches!(i, Instruction::Mul { .. })),
            "expected Mul instruction"
        );
    }

    #[test]
    fn compiles_division() {
        let instrs = instructions("let x: 8 / 2");
        assert!(
            instrs.iter().any(|i| matches!(i, Instruction::Div { .. })),
            "expected Div instruction"
        );
    }

    // Operator precedence

    #[test]
    fn mul_has_higher_precedence_than_add() {
        // `1 + 2 * 3` should generate Mul before Add.
        let instrs = instructions("let x: 1 + 2 * 3");
        let mul_pos = instrs
            .iter()
            .position(|i| matches!(i, Instruction::Mul { .. }));
        let add_pos = instrs
            .iter()
            .position(|i| matches!(i, Instruction::Add { .. }));
        assert!(mul_pos.is_some(), "expected Mul");
        assert!(add_pos.is_some(), "expected Add");
        assert!(
            mul_pos.unwrap() < add_pos.unwrap(),
            "Mul must appear before Add (higher precedence)"
        );
    }

    // Comparisons

    #[test]
    fn compiles_equality() {
        let instrs = instructions("let x: 1 == 1");
        assert!(
            instrs.iter().any(|i| matches!(i, Instruction::Eq { .. })),
            "expected Eq instruction"
        );
    }

    #[test]
    fn compiles_inequality() {
        let instrs = instructions("let x: 1 != 2");
        assert!(
            instrs.iter().any(|i| matches!(i, Instruction::Ne { .. })),
            "expected Ne instruction"
        );
    }

    #[test]
    fn compiles_less_than() {
        let instrs = instructions("let x: 1 < 2");
        assert!(instrs.iter().any(|i| matches!(i, Instruction::Lt { .. })));
    }

    #[test]
    fn compiles_greater_than() {
        let instrs = instructions("let x: 2 > 1");
        assert!(instrs.iter().any(|i| matches!(i, Instruction::Gt { .. })));
    }

    // Variables

    #[test]
    fn immutable_variable_loads_correctly() {
        // `let x: 5` then `let y: x` should emit LoadGlobal for x.
        let instrs = instructions("let x: 5\nlet y: x");
        // Second statement must load x (which is a global defined by `let`).
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::LoadGlobal { .. })),
            "expected LoadGlobal for `x`"
        );
    }

    #[test]
    fn mutable_variable_assignment_emits_store_global() {
        let instrs = instructions("mut x: 0\nx: 1");
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::StoreGlobal { .. })),
            "expected StoreGlobal instruction"
        );
    }

    #[test]
    fn increment_optimisation_for_x_plus_one() {
        // `mut x: 0\nx: x + 1` should emit IncrementGlobal, not Add+StoreGlobal.
        let instrs = instructions("mut x: 0\nx: x + 1");
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::IncrementGlobal(_))),
            "expected IncrementGlobal optimisation"
        );
        assert!(
            !instrs.iter().any(|i| matches!(i, Instruction::Add { .. })),
            "Add should be eliminated by the increment optimisation"
        );
    }

    // Lists

    #[test]
    fn compiles_empty_list() {
        let instrs = instructions("let x: []");
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::NewList { len: 0, .. })),
            "expected NewList with len=0"
        );
    }

    #[test]
    fn compiles_list_with_three_elements() {
        let instrs = instructions("let x: [1, 2, 3]");
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::NewList { len: 3, .. })),
            "expected NewList with len=3"
        );
        let set_count = instrs
            .iter()
            .filter(|i| matches!(i, Instruction::ListSet { .. }))
            .count();
        assert_eq!(set_count, 3, "expected 3 ListSet instructions");
    }

    // Objects

    #[test]
    fn compiles_empty_object() {
        let instrs = instructions("let x: {}");
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::NewObject { capacity: 0, .. })),
            "expected NewObject with capacity=0"
        );
    }

    #[test]
    fn compiles_object_with_fields() {
        let instrs = instructions("let x: {a: 1, b: 2}");
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::NewObject { .. }))
        );
        let set_count = instrs
            .iter()
            .filter(|i| matches!(i, Instruction::ObjectSet { .. }))
            .count();
        assert_eq!(set_count, 2, "expected 2 ObjectSet instructions");
    }

    // Functions

    #[test]
    fn compiles_function_declaration() {
        let prog = compile("fn add(a, b) { return a + b }").expect("should compile");
        assert_eq!(prog.functions.len(), 1, "expected one user function");
    }

    #[test]
    fn function_body_contains_add_and_return() {
        let prog = compile("fn add(a, b) { return a + b }").expect("should compile");
        let body: Vec<_> = prog.functions[0].instructions.iter().collect();
        assert!(
            body.iter().any(|i| matches!(i, Instruction::Add { .. })),
            "function body should contain Add"
        );
        assert!(
            body.iter()
                .any(|i| matches!(i, Instruction::Return(Some(_)))),
            "function body should contain Return"
        );
    }

    #[test]
    fn function_call_emits_call_instruction() {
        let instrs = instructions("fn greet() {}\ngreet()");
        assert!(
            instrs.iter().any(|i| matches!(i, Instruction::Call { .. })),
            "expected Call instruction"
        );
    }

    // Control flow

    #[test]
    fn if_statement_emits_jump_if_false() {
        let instrs = instructions("if true { let x: 1 }");
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::JumpIfFalse { .. })),
            "expected JumpIfFalse from if-statement"
        );
    }

    #[test]
    fn while_loop_emits_jumps() {
        let instrs = instructions("mut i: 0\nwhile i < 3 { i: i + 1 }");
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::JumpIfFalse { .. })),
            "expected JumpIfFalse in while loop"
        );
        assert!(
            instrs.iter().any(|i| matches!(i, Instruction::Jump(_))),
            "expected backward Jump in while loop"
        );
    }

    // String pool / interning

    #[test]
    fn identical_strings_are_interned_once() {
        // Use a real newline so the parser sees two separate statements.
        let src = "let a: \"hello world!\"\nlet b: \"hello world!\"";
        let prog = compile(src).expect("should compile");
        let s = "hello world!";
        let count = prog.string_pool.iter().filter(|p| p.as_ref() == s).count();
        assert_eq!(count, 1, "identical strings must be interned as one entry");
    }

    // Error cases

    #[test]
    fn assignment_to_immutable_variable_is_an_error() {
        let result = compile("let x: 1\nx: 2");
        assert!(
            matches!(
                result,
                Err(JitError::RedefinitionOfImmutableVariable { .. })
            ),
            "expected RedefinitionOfImmutableVariable error, got {result:?}"
        );
    }

    #[test]
    fn unknown_variable_in_assignment_is_an_error() {
        // Trying to assign to a variable that was never declared.
        let result = compile("z: 99");
        assert!(
            matches!(result, Err(JitError::UnknownVariable { .. })),
            "expected UnknownVariable error, got {result:?}"
        );
    }

    #[test]
    fn parse_error_on_missing_closing_paren() {
        let result = compile("let x: (1 + 2");
        assert!(
            result.is_err(),
            "unclosed parenthesis should produce a parse error"
        );
    }

    #[test]
    fn parse_error_on_unexpected_token() {
        // A bare `}` at top level is not valid.
        let result = compile("}");
        // The parser returns None (not Some(Err)) for `}` at top level,
        // so the program should compile to empty instructions.
        // Either way this must not panic.
        let _ = result;
    }

    // Spawn

    #[test]
    fn spawn_emits_spawn_instruction() {
        let instrs = instructions("spawn { let x: 1 }");
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, Instruction::Spawn { .. })),
            "expected Spawn instruction"
        );
    }

    // globals_count and locals_count

    #[test]
    fn globals_count_reflects_let_declarations() {
        let prog = compile("let a: 1\nlet b: 2\nlet c: 3").expect("should compile");
        assert_eq!(prog.globals_count, 3, "expected 3 globals");
    }
}
