// External scanner for tree-sitter-rsx: Rust and CSS fragments are consumed as opaque tokens.
#include "tree_sitter/parser.h"
#include <stdbool.h>
#include <string.h>

enum Token { RUST_TOPLEVEL, RUST_SETUP, RUST_EXPR, RUST_HEAD, RUST_FOR_HEAD, CSS_CONTENT, STRING_TEXT, RUST_TYPE };

void *tree_sitter_rsx_external_scanner_create(void) { return NULL; }
void tree_sitter_rsx_external_scanner_destroy(void *p) {}
unsigned tree_sitter_rsx_external_scanner_serialize(void *p, char *b) { return 0; }
void tree_sitter_rsx_external_scanner_deserialize(void *p, const char *b, unsigned n) {}

static inline void adv(TSLexer *l) { l->advance(l, false); }
static inline bool eof(TSLexer *l) { return l->eof(l); }
static bool is_ws(int32_t c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r'; }
static bool is_id_start(int32_t c) { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c == '_' || c > 127; }
static bool is_id(int32_t c) { return is_id_start(c) || (c >= '0' && c <= '9'); }

// After the opening quote: consume up to and including the closing one.
static void skip_string(TSLexer *l, int32_t q) {
  while (!eof(l)) {
    if (l->lookahead == '\\') { adv(l); if (!eof(l)) adv(l); continue; }
    if (l->lookahead == q) { adv(l); return; }
    adv(l);
  }
}

static void skip_raw_string(TSLexer *l) {  // at the first '#' or '"' after `r`
  int hashes = 0;
  while (l->lookahead == '#') { hashes++; adv(l); }
  if (l->lookahead != '"') return;
  adv(l);
  while (!eof(l)) {
    if (l->lookahead == '"') {
      adv(l);
      int n = 0;
      while (n < hashes && l->lookahead == '#') { n++; adv(l); }
      if (n == hashes) return;
    } else adv(l);
  }
}

// At '\'': a char literal or a lifetime.
static void skip_char_or_lifetime(TSLexer *l) {
  adv(l);
  if (l->lookahead == '\\') { adv(l); while (!eof(l) && l->lookahead != '\'') adv(l); if (!eof(l)) adv(l); return; }
  if (eof(l)) return;
  adv(l);
  if (l->lookahead == '\'') { adv(l); return; }
  while (is_id(l->lookahead)) adv(l);
}

typedef enum { STOP_UNMATCHED, STOP_VIEW, STOP_COMPONENT, STOP_BRACE, STOP_KEY } Stop;

// Reads an identifier into buf (truncated), consuming it all.
static int read_ident(TSLexer *l, char *buf, int cap) {
  int n = 0;
  while (is_id(l->lookahead)) { if (n < cap - 1) buf[n] = (char)l->lookahead; n++; adv(l); }
  buf[n < cap ? n : cap - 1] = 0;
  return n;
}

// Core: consume Rust (or CSS when `css`) text until a stop condition at nesting depth 0.
// The token ends (mark_end) where the stop begins. Returns true if a non-empty token was made.
static bool scan(TSLexer *l, Stop stop, bool css) {
  int depth = 0;
  bool prev_pub = false;
  bool any = false;
  bool marked = false;  // the token end has moved past at least one character
  bool line_start = true;  // only whitespace so far on this line
  l->mark_end(l);
  while (!eof(l)) {
    int32_t c = l->lookahead;
    bool at_line_start = line_start;
    if (c == '\n') line_start = true;
    else if (!is_ws(c)) line_start = false;
    if (c == '"' || (css && c == '\'')) { adv(l); skip_string(l, c); any = true; marked = true; l->mark_end(l); continue; }
    if (c == '\'' && !css) { skip_char_or_lifetime(l); any = true; marked = true; l->mark_end(l); continue; }
    if (c == '/') {
      adv(l);
      if (l->lookahead == '/' || l->lookahead == '*') {
        // comment: re-enter via a small inline copy
        if (l->lookahead == '/') { while (!eof(l) && l->lookahead != '\n') adv(l); }
        else {
          adv(l);
          int d = 1;
          while (!eof(l) && d > 0) {
            if (l->lookahead == '/') { adv(l); if (l->lookahead == '*') { adv(l); d++; } }
            else if (l->lookahead == '*') { adv(l); if (l->lookahead == '/') { adv(l); d--; } }
            else adv(l);
          }
        }
      }
      any = true; marked = true; l->mark_end(l); continue;
    }
    if (c == '(' || c == '[' || c == '{') {
      if (c == '{' && depth == 0 && stop == STOP_BRACE) return marked;
      if (c == '{' && depth == 0 && stop == STOP_KEY) return marked;
      depth++; adv(l); any = true; marked = true; l->mark_end(l); prev_pub = false; continue;
    }
    if (c == ')' || c == ']' || c == '}') {
      if (depth == 0) return marked;
      depth--; adv(l); any = true; marked = true; l->mark_end(l); prev_pub = false; continue;
    }
    if (!css && is_id_start(c)) {
      bool boundary_free = depth == 0;
      char id[16];
      // mark before the identifier so a stop keeps the token short of it
      if (boundary_free && !prev_pub) l->mark_end(l);
      int n = read_ident(l, id, sizeof id);
      if (n == 1 && (id[0] == 'r' || id[0] == 'b' || id[0] == 'c') && (l->lookahead == '"' || (id[0] == 'r' && l->lookahead == '#'))) {
        if (id[0] == 'r') skip_raw_string(l); else { adv(l); skip_string(l, '"'); }
        any = true; marked = true; l->mark_end(l); continue;
      }
      if (n == 2 && id[0] == 'b' && id[1] == 'r' && (l->lookahead == '"' || l->lookahead == '#')) { skip_raw_string(l); any = true; marked = true; l->mark_end(l); continue; }
      if (boundary_free) {
        if (stop == STOP_COMPONENT && n == 9 && strcmp(id, "component") == 0) {
          // `component Name`, or a `component` that starts a line and is not finished yet (a name still to type, a
          // lower-case one): it is the keyword all the same, and must not swallow the components after it
          while (is_ws(l->lookahead)) adv(l);
          if ((l->lookahead >= 'A' && l->lookahead <= 'Z') || at_line_start) return marked;
        }
        if (stop == STOP_VIEW && ((n == 4 && strcmp(id, "view") == 0) || (n == 5 && strcmp(id, "style") == 0))) {
          while (is_ws(l->lookahead)) adv(l);
          if (l->lookahead == '{') return marked;
        }
        if (stop == STOP_KEY && n == 3 && strcmp(id, "key") == 0 && any) {
          if (is_ws(l->lookahead)) return marked;
        }
        prev_pub = (n == 3 && strcmp(id, "pub") == 0);
      }
      any = true;
      // The identifier is part of the token unless a stop fired above; after `pub` the end stays
      // before it, in case `component` follows.
      if (!prev_pub) { marked = true; l->mark_end(l); }
      continue;
    }
    if (prev_pub && is_ws(c)) { adv(l); continue; }
    prev_pub = false;
    adv(l); any = true; marked = true; l->mark_end(l);
  }
  return marked;
}

// A type in a parameter list: up to a `,` or `)` outside any brackets. `<` `>` nest too, except the
// `>` of `->`. Trailing whitespace is left out of the token.
static bool scan_type(TSLexer *l) {
  int depth = 0;
  bool marked = false;
  l->mark_end(l);
  while (!eof(l)) {
    int32_t c = l->lookahead;
    if (depth == 0 && (c == ',' || c == ')' || c == '>' || c == '{')) return marked;
    if (c == '"') { adv(l); skip_string(l, c); marked = true; l->mark_end(l); continue; }
    if (c == '\'') { skip_char_or_lifetime(l); marked = true; l->mark_end(l); continue; }
    if (c == '-') {
      adv(l);
      if (l->lookahead == '>') adv(l);
      marked = true; l->mark_end(l); continue;
    }
    if (c == '(' || c == '[' || c == '{' || c == '<') depth++;
    else if (c == ')' || c == ']' || c == '}' || c == '>') depth--;
    adv(l);
    if (!is_ws(c)) { marked = true; l->mark_end(l); }
  }
  return marked;
}

bool tree_sitter_rsx_external_scanner_scan(void *p, TSLexer *l, const bool *valid) {
  if (valid[STRING_TEXT]) {
    // text inside a string literal, up to a quote, interpolation or escape
    bool any = false;
    while (!eof(l)) {
      int32_t c = l->lookahead;
      if (c == '"' || c == '\\') break;
      if (c == '{') {
        l->mark_end(l);
        adv(l);
        if (l->lookahead == '{') { adv(l); any = true; l->mark_end(l); continue; }
        return any && (l->result_symbol = STRING_TEXT, true);
      }
      adv(l); any = true;
    }
    if (any) { l->mark_end(l); l->result_symbol = STRING_TEXT; }
    return any;
  }
  // Rust and CSS tokens do not start with whitespace.
  while (is_ws(l->lookahead)) l->advance(l, true);
  if (valid[RUST_TYPE]) {
    if (!scan_type(l)) return false;
    l->result_symbol = RUST_TYPE;
    return true;
  }
  Stop stop;
  enum Token tok;
  bool css = false;
  if (valid[RUST_TOPLEVEL]) { tok = RUST_TOPLEVEL; stop = STOP_COMPONENT; }
  else if (valid[RUST_SETUP]) { tok = RUST_SETUP; stop = STOP_VIEW; }
  else if (valid[CSS_CONTENT]) { tok = CSS_CONTENT; stop = STOP_UNMATCHED; css = true; }
  else if (valid[RUST_FOR_HEAD]) { tok = RUST_FOR_HEAD; stop = STOP_KEY; }
  else if (valid[RUST_HEAD]) { tok = RUST_HEAD; stop = STOP_BRACE; }
  else if (valid[RUST_EXPR]) { tok = RUST_EXPR; stop = STOP_UNMATCHED; }
  else return false;
  if (tok == RUST_TOPLEVEL && eof(l)) return false;
  if (!scan(l, stop, css)) return false;
  l->result_symbol = tok;
  return true;
}
