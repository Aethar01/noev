// The one token the grammar cannot describe on its own: a `<` that starts a line, which
// starts a new statement (an expectation value) rather than continuing the previous line as
// a comparison (spec/grammar.md §1, Line breaks). A bra at the start of a line (`<1| mv q`)
// is left to the ordinary lexer.

#include "tree_sitter/parser.h"

enum TokenType {
    LINE_START_ANGLE,
};

void *tree_sitter_noev_external_scanner_create(void) { return NULL; }

void tree_sitter_noev_external_scanner_destroy(void *payload) {}

unsigned tree_sitter_noev_external_scanner_serialize(void *payload, char *buffer) { return 0; }

void tree_sitter_noev_external_scanner_deserialize(void *payload, const char *buffer,
                                                   unsigned length) {}

static bool is_ket_char(int32_t c) { return c == '0' || c == '1' || c == '+' || c == '-'; }

bool tree_sitter_noev_external_scanner_scan(void *payload, TSLexer *lexer,
                                            const bool *valid_symbols) {
    if (!valid_symbols[LINE_START_ANGLE]) {
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
    if (!line_break || lexer->lookahead != '<') {
        return false;
    }
    lexer->advance(lexer, false);
    lexer->mark_end(lexer);
    // `<01|` is a bra, and `<<` a shift; neither is this token.
    if (lexer->lookahead == '<') {
        return false;
    }
    bool body = false;
    while (is_ket_char(lexer->lookahead)) {
        body = true;
        lexer->advance(lexer, false);
    }
    if (body && lexer->lookahead == '|') {
        return false;
    }
    lexer->result_symbol = LINE_START_ANGLE;
    return true;
}
