use super::GrammarRule;
use crate::parser::Span;
use yara_derive::Error;

/// An error occurred while parsing YARA rules.
/// 
/// Each error variant has a `detailed_report` field, which contains a detailed
/// text-mode error report like this one ...
/// 
/// ```text
/// error: duplicate tag `tag1`
///    ╭─[line:1:18]
///    │
///  1 │ rule test : tag1 tag1 { condition: true }
///    ·                  ──┬─  
///    ·                    ╰─── duplicate tag
/// ───╯
/// ```
/// 
/// Each variant also contains additional pieces of information that are 
/// relevant for that specific error. This information is usually contained
/// inside the detailed report itself, but having access to the individual 
/// pieces is useful for applications that can't rely on text-based reports.
/// 
#[rustfmt::skip]
#[derive(Error)]
pub enum Error {
    #[error("syntax error")]
    #[label("{error_msg}", error_span)]
    SyntaxError { 
        detailed_report: String, 
        error_msg: String, 
        error_span: Span 
    },

    #[error("duplicate tag `{tag}`")]
    #[label("duplicate tag", tag_span)]
    DuplicateTag {
        detailed_report: String,
        tag: String,
        tag_span: Span,
    },

    #[error("duplicate rule `{rule_ident}`")]
    #[label(
        "duplicate declaration of `{rule_ident}`", 
        new_rule_name_span
    )]
    #[label(
        "`{rule_ident}` declared here for the first time",
        existing_rule_name_span,
        style="note"
    )]
    DuplicateRule {
        detailed_report: String,
        rule_ident: String,
        new_rule_name_span: Span,
        existing_rule_name_span: Span,
    },
    
    #[error("duplicate string `{string_ident}`")]
    #[label(
        "duplicate declaration of `{string_ident}`", 
        new_string_span
    )]
    #[label(
        "`{string_ident}` declared here for the first time", 
        existing_string_span,
        style="note"
    )]
    DuplicateString {
        detailed_report: String,
        string_ident: String,
        new_string_span: Span,
        existing_string_span: Span,
    },

    #[error("invalid string modifier")]
    #[label("{error_msg}", error_span)]
    InvalidModifier {
        detailed_report: String,
        error_msg: String,
        error_span: Span,
    },

    #[error("duplicate string modifier")]
    #[label("duplicate modifier", modifier_span)]
    DuplicateModifier {
        detailed_report: String,
        modifier_span: Span,
    },

    #[error(
        "invalid string modifier combination: `{modifier1}` `{modifier2}`", 
    )]
    #[label("`{modifier1}` modifier used here", modifier1_span)]
    #[label("`{modifier2}` modifier used here", modifier2_span)]
    #[note(note)]
    InvalidModifierCombination {
        detailed_report: String,
        modifier1: String,
        modifier2: String,
        modifier1_span: Span,
        modifier2_span: Span,
        note: Option<String>,
    },

    #[error("unused string `{string_ident}`")]
    #[label("this was not used in the condition", string_ident_span)]
    UnusedString {
        detailed_report: String, 
        string_ident: String, 
        string_ident_span: Span,
    },

    #[error("invalid hex string `{string_ident}`")]
    #[label("{error_msg}", error_span)]
    #[note(note)]
    InvalidHexString {
        detailed_report: String,
        string_ident: String,
        error_msg: String,
        error_span: Span,
        note: Option<String>,
    },

    #[error("invalid range")]
    #[label("{error_msg}", error_span)]
    InvalidRange {
        detailed_report: String,
        error_msg: String,
        error_span: Span,
    },

    #[error("invalid integer")]
    #[label("{error_msg}", error_span)]
    InvalidInteger {
        detailed_report: String,
        error_msg: String,
        error_span: Span,
    },

    #[error("invalid float")]
    #[label("{error_msg}", error_span)]
    InvalidFloat {
        detailed_report: String,
        error_msg: String,
        error_span: Span,
    },

    #[error("invalid escape sequence")]
    #[label("{error_msg}", error_span)]
    InvalidEscapeSequence {
        detailed_report: String,
        error_msg: String,
        error_span: Span,
    },
}

impl Error {
    pub fn syntax_error_message<F>(
        expected: &[GrammarRule],
        unexpected: &[GrammarRule],
        mut f: F,
    ) -> String
    where
        F: FnMut(&GrammarRule) -> &str,
    {
        // Remove COMMENT and WHITESPACE from the lists of expected and not
        // expected rules. We don't want error messages like:
        //
        //    expected identifier or COMMENT
        //    expected { or WHITESPACE
        //
        // The alternative solution is silencing those rules in grammar.pest,
        // but that means that Pest will completely ignore them and we won't
        // get comments nor spaces in the parse tree. We want those rules in
        // the parse tree, but we don't want them in error messages. This is
        // probably an area of improvement for Pest.
        let expected: Vec<&GrammarRule> = expected
            .iter()
            .filter(|&&r| {
                r != GrammarRule::COMMENT && r != GrammarRule::WHITESPACE
            })
            .collect();

        let unexpected: Vec<&GrammarRule> = unexpected
            .iter()
            .filter(|&&r| {
                r != GrammarRule::COMMENT && r != GrammarRule::WHITESPACE
            })
            .collect();

        match (unexpected.is_empty(), expected.is_empty()) {
            (false, false) => format!(
                "unexpected {}; expected {}",
                Self::enumerate_grammar_rules(&unexpected, &mut f),
                Self::enumerate_grammar_rules(&expected, &mut f)
            ),
            (false, true) => {
                format!(
                    "unexpected {}",
                    Self::enumerate_grammar_rules(&unexpected, &mut f)
                )
            }
            (true, false) => {
                format!(
                    "expected {}",
                    Self::enumerate_grammar_rules(&expected, &mut f)
                )
            }
            (true, true) => "unknown parsing error".to_owned(),
        }
    }

    pub fn enumerate_grammar_rules<F>(
        rules: &[&GrammarRule],
        f: &mut F,
    ) -> String
    where
        F: FnMut(&GrammarRule) -> &str,
    {
        // All grammar rules in `rules` are mapped using `f`.
        let mut strings = rules.iter().map(|rule| f(rule)).collect::<Vec<_>>();

        // Sort alphabetically.
        strings.sort();

        // Deduplicate repeated items.
        strings.dedup();

        match strings.len() {
            1 => strings[0].to_owned(),
            2 => format!("{} or {}", strings[0], strings[1]),
            l => {
                format!(
                    "{}, or {}",
                    strings[..l - 1].join(", "),
                    strings[l - 1]
                )
            }
        }
    }

    /// Given a grammar rule returns a more appropriate string that will be used
    /// in error messages.
    pub fn printable_string(rule: &GrammarRule) -> &str {
        match rule {
            // Keywords
            GrammarRule::k_ALL => "`all`",
            GrammarRule::k_ANY => "`any`",
            GrammarRule::k_ASCII => "`ascii`",
            GrammarRule::k_AT => "`at`",
            GrammarRule::k_BASE64 => "`base64`",
            GrammarRule::k_BASE64WIDE => "`base64wide`",
            GrammarRule::k_CONDITION => "`condition`",
            GrammarRule::k_FALSE => "`false`",
            GrammarRule::k_FILESIZE => "`filesize`",
            GrammarRule::k_FOR => "`for`",
            GrammarRule::k_FULLWORD => "`fullword`",
            GrammarRule::k_GLOBAL => "`global`",
            GrammarRule::k_IMPORT => "`import`",
            GrammarRule::k_IN => "`in`",
            GrammarRule::k_META => "`meta`",
            GrammarRule::k_NOCASE => "`nocase`",
            GrammarRule::k_NOT => "`not`",
            GrammarRule::k_OF => "`of`",
            GrammarRule::k_PRIVATE => "`private`",
            GrammarRule::k_RULE => "`rule`",

            GrammarRule::k_STRINGS => "`strings`",
            GrammarRule::k_THEM => "`them`",
            GrammarRule::k_TRUE => "`true`",
            GrammarRule::k_WIDE => "`wide`",
            GrammarRule::k_XOR => "`xor`",

            GrammarRule::boolean_expr | GrammarRule::boolean_term => {
                "boolean expression"
            }
            GrammarRule::expr
            | GrammarRule::primary_expr
            | GrammarRule::term => "expression",

            GrammarRule::hex_byte => "byte",
            GrammarRule::hex_pattern => "bytes",
            GrammarRule::ident => "identifier",
            GrammarRule::integer_lit => "number",
            GrammarRule::float_lit => "number",
            GrammarRule::rule_decl => "rule declaration",
            GrammarRule::source_file => "YARA rules",
            GrammarRule::string_ident => "string identifier",
            GrammarRule::string_lit => "string literal",
            GrammarRule::regexp => "regular expression",
            GrammarRule::string_mods => "string modifiers",

            GrammarRule::PERCENT => "percent `%`",
            GrammarRule::MINUS => "`-`",
            GrammarRule::COLON => "colon `:`",

            GrammarRule::ADD
            | GrammarRule::k_AND
            | GrammarRule::k_OR
            | GrammarRule::SUB
            | GrammarRule::DIV
            | GrammarRule::MUL
            | GrammarRule::MOD
            | GrammarRule::SHL
            | GrammarRule::SHR
            | GrammarRule::BITWISE_AND
            | GrammarRule::BITWISE_OR
            | GrammarRule::BITWISE_XOR
            | GrammarRule::EQ
            | GrammarRule::NEQ
            | GrammarRule::GE
            | GrammarRule::GT
            | GrammarRule::LE
            | GrammarRule::LT
            | GrammarRule::k_STARTSWITH
            | GrammarRule::k_ISTARTSWITH
            | GrammarRule::k_ENDSWITH
            | GrammarRule::k_IENDSWITH
            | GrammarRule::k_IEQUALS
            | GrammarRule::k_CONTAINS
            | GrammarRule::k_ICONTAINS
            | GrammarRule::k_MATCHES => "operator",

            GrammarRule::PIPE => "pipe `|`",
            GrammarRule::COMMA => "comma `,`",
            GrammarRule::DOT => "dot `.`",
            GrammarRule::EQUAL => "equal `=` ",

            GrammarRule::LPAREN => "opening parenthesis `(`",
            GrammarRule::RPAREN => "closing parenthesis `)`",
            GrammarRule::LBRACE => "opening brace `{`",
            GrammarRule::RBRACE => "closing brace `}`",
            GrammarRule::LBRACKET => "opening bracket `[`",
            GrammarRule::RBRACKET => "closing bracket `]`",
            GrammarRule::EOI => "end of file",

            _ => unreachable!("case `{:?}` is not handled", rule),
        }
    }
}
