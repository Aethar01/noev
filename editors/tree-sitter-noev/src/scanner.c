// The tokens the grammar cannot describe on its own: a `<` or `-` that starts a line, which
// starts something new (an expectation value, a negative number, a `match` arm's `-1`)
// rather than continuing the previous line as a comparison or a subtraction
// (spec/grammar.md §1, Line breaks). A bra at the start of a line (`<1| mv q`), `->`, `-=`
// and `<<` are left to the ordinary lexer.

#include "tree_sitter/parser.h"

enum TokenType {
    LINE_START_ANGLE,
    LINE_START_MINUS,
};

void *tree_sitter_noev_external_scanner_create(void) { return NULL; }

void tree_sitter_noev_external_scanner_destroy(void *payload) {}

unsigned tree_sitter_noev_external_scanner_serialize(void *payload, char *buffer) { return 0; }

void tree_sitter_noev_external_scanner_deserialize(void *payload, const char *buffer,
                                                   unsigned length) {}

static bool is_ket_char(int32_t c) { return c == '0' || c == '1' || c == '+' || c == '-'; }

bool tree_sitter_noev_external_scanner_scan(void *payload, TSLexer *lexer,
                                            const bool *valid_symbols) {
    if (!valid_symbols[LINE_START_ANGLE] && !valid_symbols[LINE_START_MINUS]) {
        return false;
    }
    bool line_break = false;
    while (lexer->lookahead == ' ' || lexer->lookahead == '\t' || lexer->lookahead == '\r' ||
           lexer->lookahead == '\n') {
        if (lexer->lookahead == '\n') {
            line_break = true;
        }
        lexer->advance(lexer, true);
    }
    if (!line_break) {
        return false;
    }
    if (lexer->lookahead == '-' && valid_symbols[LINE_START_MINUS]) {
        lexer->advance(lexer, false);
        lexer->mark_end(lexer);
        if (lexer->lookahead == '>' || lexer->lookahead == '=') {
            return false;
        }
        lexer->result_symbol = LINE_START_MINUS;
        return true;
    }
    if (lexer->lookahead != '<' || !valid_symbols[LINE_START_ANGLE]) {
        return false;
    }
    lexer->advance(lexer, false);
    lexer->mark_end(lexer);
    // `<01|` is a bra, and `<<` a shift; neither is this token.
    if (lexer->lookahead == '<') {
        return false;
    }
    // Labels: `0 1 + -`, `(+i)` and `(-i)`, and computed ones `{expr}`.
    bool body = false;
    for (;;) {
        if (is_ket_char(lexer->lookahead)) {
            lexer->advance(lexer, false);
        } else if (lexer->lookahead == '{') {
            // A computed label, `{expr}`, with braces nested inside.
            int depth = 0;
            do {
                if (lexer->lookahead == '{') depth++;
                if (lexer->lookahead == '}') depth--;
                if (lexer->eof(lexer)) return false;
                lexer->advance(lexer, false);
            } while (depth > 0);
        } else if (lexer->lookahead == '(') {
            lexer->advance(lexer, false);
            if (lexer->lookahead != '+' && lexer->lookahead != '-') break;
            lexer->advance(lexer, false);
            if (lexer->lookahead != 'i') break;
            lexer->advance(lexer, false);
            if (lexer->lookahead != ')') break;
            lexer->advance(lexer, false);
        } else {
            break;
        }
        body = true;
    }
    if (body && lexer->lookahead == '|') {
        return false;
    }
    lexer->result_symbol = LINE_START_ANGLE;
    return true;
}
