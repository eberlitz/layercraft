//! Safe clean-room G-code macro template evaluator supporting `[placeholder]`
//! substitutions, indexed vectors, arithmetic/logical `{expr}` evaluation, and
//! `{if ...} ... {elsif ...} ... {else} ... {endif}` conditionals.

use serde::{Deserialize, Serialize};

const MAX_TEMPLATE_DEPTH: usize = 16;
const MAX_EXPR_DEPTH: usize = 32;

/// Template variable context for start, layer-change, and end G-code macros.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateVars {
    pub nozzle_temperature: u16,
    pub first_layer_temperature: u16,
    pub bed_temperature: u16,
    pub first_layer_bed_temperature: u16,
    pub chamber_temperature: u16,
    pub layer_num: usize,
    pub layer_z: f64,
    pub layer_height: f64,
    pub first_layer_height: f64,
    pub total_layer_count: usize,
    pub max_z: f64,
    pub bed_width: f64,
    pub bed_depth: f64,
    pub bed_height: f64,
    pub nozzle_diameter: f64,
    pub nozzle_high_flow: bool,
    pub filament_diameter: f64,
    pub filament_type: String,
    pub printer_model: String,
    pub first_layer_print_min: [f64; 2],
    pub first_layer_print_max: [f64; 2],
    pub print_min: [f64; 2],
    pub print_max: [f64; 2],
}

impl Default for TemplateVars {
    fn default() -> Self {
        Self {
            nozzle_temperature: 200,
            first_layer_temperature: 200,
            bed_temperature: 60,
            first_layer_bed_temperature: 60,
            chamber_temperature: 0,
            layer_num: 0,
            layer_z: 0.0,
            layer_height: 0.2,
            first_layer_height: 0.2,
            total_layer_count: 0,
            max_z: 0.0,
            bed_width: 250.0,
            bed_depth: 250.0,
            bed_height: 250.0,
            nozzle_diameter: 0.4,
            nozzle_high_flow: false,
            filament_diameter: 1.75,
            filament_type: String::new(),
            printer_model: String::new(),
            first_layer_print_min: [0.0, 0.0],
            first_layer_print_max: [0.0, 0.0],
            print_min: [0.0, 0.0],
            print_max: [0.0, 0.0],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum TVal {
    Num(f64),
    Bool(bool),
    Str(String),
}

impl TVal {
    fn as_bool(&self) -> bool {
        match self {
            Self::Bool(b) => *b,
            Self::Num(n) => n.abs() > 1e-12,
            Self::Str(s) => !s.is_empty() && s != "0" && !s.eq_ignore_ascii_case("false"),
        }
    }

    fn as_num(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            Self::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Self::Str(_) => None,
        }
    }

    fn to_template_string(&self) -> String {
        match self {
            Self::Num(n) => format_template_f64(*n),
            Self::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
            Self::Str(s) => s.clone(),
        }
    }
}

fn format_template_f64(val: f64) -> String {
    let s = format!("{val:.3}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed == "-0" || trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn lookup_indexed_var(base: &str, idx: Option<usize>, vars: &TemplateVars) -> Option<TVal> {
    match base {
        "first_layer_print_min" => {
            let i = idx.unwrap_or(0);
            vars.first_layer_print_min.get(i).copied().map(TVal::Num)
        }
        "first_layer_print_max" => {
            let i = idx.unwrap_or(0);
            vars.first_layer_print_max.get(i).copied().map(TVal::Num)
        }
        "print_min" => {
            let i = idx.unwrap_or(0);
            vars.print_min.get(i).copied().map(TVal::Num)
        }
        "print_max" => {
            let i = idx.unwrap_or(0);
            vars.print_max.get(i).copied().map(TVal::Num)
        }
        _ => {
            if idx.is_some_and(|i| i != 0) {
                return None;
            }
            lookup_scalar_var(base, vars)
        }
    }
}

fn lookup_scalar_var(key: &str, vars: &TemplateVars) -> Option<TVal> {
    match key {
        "nozzle_temperature" => Some(TVal::Num(f64::from(vars.nozzle_temperature))),
        "first_layer_temperature" | "nozzle_temperature_initial_layer" => {
            Some(TVal::Num(f64::from(vars.first_layer_temperature)))
        }
        "bed_temperature" | "hot_plate_temp" => Some(TVal::Num(f64::from(vars.bed_temperature))),
        "first_layer_bed_temperature"
        | "bed_temperature_initial_layer"
        | "bed_temperature_initial_layer_single"
        | "hot_plate_temp_initial_layer" => {
            Some(TVal::Num(f64::from(vars.first_layer_bed_temperature)))
        }
        "chamber_temperature" | "chamber_temperatures" => {
            Some(TVal::Num(f64::from(vars.chamber_temperature)))
        }
        "layer_num" => Some(TVal::Num(vars.layer_num as f64)),
        "layer_z" => Some(TVal::Num(vars.layer_z)),
        "layer_height" => Some(TVal::Num(vars.layer_height)),
        "initial_layer_print_height" => Some(TVal::Num(vars.first_layer_height)),
        "total_layer_count" | "total_layer_number" => {
            Some(TVal::Num(vars.total_layer_count as f64))
        }
        "max_z" | "max_layer_z" => Some(TVal::Num(vars.max_z)),
        "bed_width" => Some(TVal::Num(vars.bed_width)),
        "bed_depth" => Some(TVal::Num(vars.bed_depth)),
        "bed_height" => Some(TVal::Num(vars.bed_height)),
        "nozzle_diameter" => Some(TVal::Num(vars.nozzle_diameter)),
        "nozzle_high_flow" => Some(TVal::Bool(vars.nozzle_high_flow)),
        "filament_diameter" => Some(TVal::Num(vars.filament_diameter)),
        "filament_type" => Some(TVal::Str(vars.filament_type.clone())),
        "printer_model" => Some(TVal::Str(vars.printer_model.clone())),
        "first_layer_print_min_x" => Some(TVal::Num(vars.first_layer_print_min[0])),
        "first_layer_print_min_y" => Some(TVal::Num(vars.first_layer_print_min[1])),
        "first_layer_print_max_x" => Some(TVal::Num(vars.first_layer_print_max[0])),
        "first_layer_print_max_y" => Some(TVal::Num(vars.first_layer_print_max[1])),
        "first_layer_print_size_x" => Some(TVal::Num(
            (vars.first_layer_print_max[0] - vars.first_layer_print_min[0]).max(0.0),
        )),
        "first_layer_print_size_y" => Some(TVal::Num(
            (vars.first_layer_print_max[1] - vars.first_layer_print_min[1]).max(0.0),
        )),
        "print_min_x" => Some(TVal::Num(vars.print_min[0])),
        "print_min_y" => Some(TVal::Num(vars.print_min[1])),
        "print_max_x" => Some(TVal::Num(vars.print_max[0])),
        "print_max_y" => Some(TVal::Num(vars.print_max[1])),
        _ => None,
    }
}

fn lookup_bracket_key(key: &str, suffix_idx: Option<usize>, vars: &TemplateVars) -> Option<String> {
    lookup_indexed_var(key, suffix_idx, vars).map(|v| v.to_template_string())
}

/// Single-pass expansion of LayerCraft `[placeholder]`, `{if ...}`, and `{expr}`
/// macros in G-code templates. Unknown or unparseable tokens are preserved verbatim.
pub fn expand_gcode_template(template: &str, vars: &TemplateVars) -> String {
    let normalized = template.replace("\r\n", "\n");
    let after_braces = eval_braces(&normalized, vars, 0);
    expand_brackets(&after_braces, vars)
}

fn find_matching_brace(s: &str, start_after_open: usize) -> Option<usize> {
    let rest = s.get(start_after_open..)?;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    for (rel_idx, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(q) = in_quote {
            if ch == '\\' {
                escaped = true;
            } else if ch == q {
                in_quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => in_quote = Some(ch),
            '{' => return None,
            '}' => return Some(start_after_open + rel_idx),
            _ => {}
        }
    }
    None
}

enum TagKind<'a> {
    If(&'a str),
    Elsif(&'a str),
    Else,
    Endif,
    Expr(&'a str),
}

fn classify_brace_tag(inner: &str) -> TagKind<'_> {
    let t = inner.trim();
    if let Some(cond) = strip_keyword(t, "if") {
        TagKind::If(cond)
    } else if let Some(cond) = strip_keyword(t, "elsif").or_else(|| strip_keyword(t, "elseif")) {
        TagKind::Elsif(cond)
    } else if t == "else" {
        TagKind::Else
    } else if t == "endif" {
        TagKind::Endif
    } else {
        TagKind::Expr(t)
    }
}

fn strip_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(kw)?;
    match rest.chars().next() {
        Some(c) if c.is_whitespace() || c == '(' => Some(rest.trim()),
        _ => None,
    }
}

struct ParsedIfBlock<'a> {
    branches: Vec<(Option<&'a str>, &'a str)>,
    after_endif: usize,
}

fn parse_if_block<'a>(
    s: &'a str,
    first_cond: &'a str,
    body_start: usize,
) -> Option<ParsedIfBlock<'a>> {
    let mut depth = 1usize;
    let mut cursor = body_start;
    let mut cur_cond: Option<&'a str> = Some(first_cond);
    let mut cur_body_start = body_start;
    let mut branches = Vec::new();

    while let Some(slice) = s.get(cursor..) {
        let rel_open = slice.find('{')?;
        let open = cursor + rel_open;
        let Some(close) = find_matching_brace(s, open + 1) else {
            cursor = open + 1;
            continue;
        };
        let inner = s.get(open + 1..close)?;
        match classify_brace_tag(inner) {
            TagKind::If(_) => {
                depth += 1;
            }
            TagKind::Elsif(next_cond) if depth == 1 => {
                let body = s.get(cur_body_start..open)?;
                branches.push((cur_cond, body));
                cur_cond = Some(next_cond);
                cur_body_start = close + 1;
            }
            TagKind::Else if depth == 1 => {
                let body = s.get(cur_body_start..open)?;
                branches.push((cur_cond, body));
                cur_cond = None;
                cur_body_start = close + 1;
            }
            TagKind::Endif => {
                depth -= 1;
                if depth == 0 {
                    let body = s.get(cur_body_start..open)?;
                    branches.push((cur_cond, body));
                    return Some(ParsedIfBlock {
                        branches,
                        after_endif: close + 1,
                    });
                }
            }
            TagKind::Elsif(_) | TagKind::Else | TagKind::Expr(_) => {}
        }
        cursor = close + 1;
    }
    None
}

fn eval_braces(input: &str, vars: &TemplateVars, depth: usize) -> String {
    if depth >= MAX_TEMPLATE_DEPTH {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0usize;

    while let Some(slice) = input.get(cursor..) {
        let Some(rel_open) = slice.find('{') else {
            out.push_str(slice);
            break;
        };
        if let Some(prefix) = slice.get(..rel_open) {
            out.push_str(prefix);
        }
        let open = cursor + rel_open;
        let Some(close) = find_matching_brace(input, open + 1) else {
            out.push('{');
            cursor = open + 1;
            continue;
        };
        let Some(inner) = input.get(open + 1..close) else {
            out.push('{');
            cursor = open + 1;
            continue;
        };
        match classify_brace_tag(inner) {
            TagKind::If(first_cond) => {
                if let Some(block) = parse_if_block(input, first_cond, close + 1) {
                    let mut chosen: Option<&str> = None;
                    let mut eval_failed = false;
                    for (cond_opt, body) in &block.branches {
                        match cond_opt {
                            Some(cond) => match eval_expr(cond, vars) {
                                Ok(val) => {
                                    if val.as_bool() {
                                        chosen = Some(body);
                                        break;
                                    }
                                }
                                Err(()) => {
                                    eval_failed = true;
                                    break;
                                }
                            },
                            None => {
                                chosen = Some(body);
                                break;
                            }
                        }
                    }
                    if eval_failed {
                        if let Some(raw_block) = input.get(open..block.after_endif) {
                            out.push_str(raw_block);
                            cursor = block.after_endif;
                            continue;
                        }
                    } else {
                        if let Some(body) = chosen {
                            out.push_str(&eval_braces(body, vars, depth + 1));
                        }
                        cursor = block.after_endif;
                        continue;
                    }
                }
                out.push('{');
                cursor = open + 1;
            }
            TagKind::Expr(expr_str) => {
                if let Ok(val) = eval_expr(expr_str, vars) {
                    out.push_str(&val.to_template_string());
                    cursor = close + 1;
                } else if let Some(raw_tag) = input.get(open..=close) {
                    out.push_str(raw_tag);
                    cursor = close + 1;
                } else {
                    out.push('{');
                    cursor = open + 1;
                }
            }
            TagKind::Elsif(_) | TagKind::Else | TagKind::Endif => {
                if let Some(raw_tag) = input.get(open..=close) {
                    out.push_str(raw_tag);
                    cursor = close + 1;
                } else {
                    out.push('{');
                    cursor = open + 1;
                }
            }
        }
    }

    out
}

fn expand_brackets(input: &str, vars: &TemplateVars) -> String {
    let mut out = String::with_capacity(input.len() + 32);
    let mut cursor = 0usize;

    while let Some(slice) = input.get(cursor..) {
        let Some(rel_open) = slice.find('[') else {
            out.push_str(slice);
            break;
        };
        if let Some(prefix) = slice.get(..rel_open) {
            out.push_str(prefix);
        }
        let open_idx = cursor + rel_open;
        let after_open = open_idx + 1;
        let rest = input.get(after_open..).unwrap_or("");
        let close_pos = rest.find(']');
        let newline_pos = rest.find('\n');
        let nested_open = rest.find('[');

        if let Some(no) = nested_open
            && no > 0
            && newline_pos.is_none_or(|nl| no < nl)
            && let Some(tail) = rest.get(no..)
        {
            let suffix = if tail.starts_with("[0]]") {
                Some(("[0]]", 0usize))
            } else if tail.starts_with("[1]]") {
                Some(("[1]]", 1usize))
            } else if tail.starts_with("[initial_extruder]]") {
                Some(("[initial_extruder]]", 0usize))
            } else {
                None
            };
            if let Some((suf, idx_val)) = suffix
                && let Some(key) = rest.get(..no)
                && let Some(val) = lookup_bracket_key(key, Some(idx_val), vars)
            {
                out.push_str(&val);
                cursor = after_open + no + suf.len();
                continue;
            }
        }

        let valid_close = close_pos.filter(|&c| {
            newline_pos.is_none_or(|nl| c < nl) && nested_open.is_none_or(|no| c < no)
        });

        if let Some(rel_close) = valid_close {
            let key = rest.get(..rel_close).unwrap_or("");
            if let Some(val) = lookup_bracket_key(key, None, vars) {
                out.push_str(&val);
                cursor = after_open + rel_close + 1;
                continue;
            }
        }

        out.push('[');
        cursor = after_open;
    }

    out
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String, Option<usize>),
    Plus,
    Minus,
    Star,
    Slash,
    Bang,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    LParen,
    RParen,
    Comma,
}

fn tokenize_expr(input: &str) -> Result<Vec<Tok>, ()> {
    let mut toks = Vec::new();
    let mut i = 0usize;
    let bytes_len = input.len();

    while i < bytes_len {
        let Some(rest) = input.get(i..) else {
            return Err(());
        };
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            i += ch.len_utf8();
            continue;
        }
        if rest.starts_with("||") {
            toks.push(Tok::OrOr);
            i += 2;
            continue;
        }
        if rest.starts_with("&&") {
            toks.push(Tok::AndAnd);
            i += 2;
            continue;
        }
        if rest.starts_with("==") {
            toks.push(Tok::EqEq);
            i += 2;
            continue;
        }
        if rest.starts_with("!=") {
            toks.push(Tok::NotEq);
            i += 2;
            continue;
        }
        if rest.starts_with("<=") {
            toks.push(Tok::Le);
            i += 2;
            continue;
        }
        if rest.starts_with(">=") {
            toks.push(Tok::Ge);
            i += 2;
            continue;
        }
        match ch {
            '+' => {
                toks.push(Tok::Plus);
                i += 1;
                continue;
            }
            '-' => {
                toks.push(Tok::Minus);
                i += 1;
                continue;
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
                continue;
            }
            '/' => {
                toks.push(Tok::Slash);
                i += 1;
                continue;
            }
            '!' => {
                toks.push(Tok::Bang);
                i += 1;
                continue;
            }
            '<' => {
                toks.push(Tok::Lt);
                i += 1;
                continue;
            }
            '>' => {
                toks.push(Tok::Gt);
                i += 1;
                continue;
            }
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
                continue;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
                continue;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
                continue;
            }
            '"' | '\'' => {
                let mut lit = String::new();
                let mut escaped = false;
                let mut closed_at = None;
                let inner_rest = input.get(i + 1..).ok_or(())?;
                for (rel, c) in inner_rest.char_indices() {
                    if escaped {
                        lit.push(c);
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == ch {
                        closed_at = Some(i + 1 + rel + c.len_utf8());
                        break;
                    } else {
                        lit.push(c);
                    }
                }
                let next_pos = closed_at.ok_or(())?;
                toks.push(Tok::Str(lit));
                i = next_pos;
                continue;
            }
            _ => {}
        }

        if ch.is_ascii_digit()
            || (ch == '.' && rest.chars().nth(1).is_some_and(|c| c.is_ascii_digit()))
        {
            let mut end = 0usize;
            let mut seen_dot = false;
            for (rel, c) in rest.char_indices() {
                if c.is_ascii_digit() {
                    end = rel + 1;
                } else if c == '.' && !seen_dot {
                    seen_dot = true;
                    end = rel + 1;
                } else {
                    break;
                }
            }
            let num_str = rest.get(..end).ok_or(())?;
            let val = num_str.parse::<f64>().map_err(|_| ())?;
            if !val.is_finite() {
                return Err(());
            }
            toks.push(Tok::Num(val));
            i += end;
            continue;
        }

        if ch == '[' {
            let bracket_rest = input.get(i + 1..).ok_or(())?;
            let (base_ident, idx_opt, consumed) = parse_bracket_ident_in_expr(bracket_rest)?;
            toks.push(Tok::Ident(base_ident, idx_opt));
            i += 1 + consumed;
            continue;
        }

        if ch.is_ascii_alphabetic() || ch == '_' {
            let mut end = 0usize;
            for (rel, c) in rest.char_indices() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    end = rel + c.len_utf8();
                } else {
                    break;
                }
            }
            let ident = rest.get(..end).ok_or(())?;
            let mut consumed = end;
            let mut idx_opt = None;
            if let Some(after_ident) = rest.get(end..)
                && after_ident.starts_with('[')
            {
                let (idx_parsed, bracket_len) = parse_index_suffix(after_ident)?;
                idx_opt = Some(idx_parsed);
                consumed += bracket_len;
            }
            toks.push(Tok::Ident(ident.to_owned(), idx_opt));
            i += consumed;
            continue;
        }

        return Err(());
    }

    if toks.len() > 256 {
        return Err(());
    }
    Ok(toks)
}

fn parse_index_suffix(after_ident: &str) -> Result<(usize, usize), ()> {
    let close = after_ident.find(']').ok_or(())?;
    let inside = after_ident.get(1..close).ok_or(())?.trim();
    let idx = if inside == "initial_extruder" {
        0usize
    } else {
        inside.parse::<usize>().map_err(|_| ())?
    };
    Ok((idx, close + 1))
}

fn parse_bracket_ident_in_expr(after_open: &str) -> Result<(String, Option<usize>, usize), ()> {
    let close = after_open.find(']').ok_or(())?;
    let first_part = after_open.get(..close).ok_or(())?;
    if let Some((base, idx_open)) = first_part.split_once('[') {
        let idx_str = idx_open.trim();
        let after_first = after_open.get(close + 1..).ok_or(())?;
        if !after_first.starts_with(']') {
            return Err(());
        }
        let idx = if idx_str == "initial_extruder" {
            0usize
        } else {
            idx_str.parse::<usize>().map_err(|_| ())?
        };
        Ok((base.trim().to_owned(), Some(idx), close + 2))
    } else {
        Ok((first_part.trim().to_owned(), None, close + 1))
    }
}

struct ExprParser<'a> {
    toks: &'a [Tok],
    pos: usize,
    vars: &'a TemplateVars,
}

impl<'a> ExprParser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.pos)
    }

    fn bump(&mut self) -> Option<&'a Tok> {
        let tok = self.toks.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn parse_or(&mut self, depth: usize) -> Result<TVal, ()> {
        if depth >= MAX_EXPR_DEPTH {
            return Err(());
        }
        let mut lhs = self.parse_and(depth + 1)?;
        while matches!(self.peek(), Some(Tok::OrOr)) {
            self.bump();
            let rhs = self.parse_and(depth + 1)?;
            lhs = TVal::Bool(lhs.as_bool() || rhs.as_bool());
        }
        Ok(lhs)
    }

    fn parse_and(&mut self, depth: usize) -> Result<TVal, ()> {
        let mut lhs = self.parse_eq(depth + 1)?;
        while matches!(self.peek(), Some(Tok::AndAnd)) {
            self.bump();
            let rhs = self.parse_eq(depth + 1)?;
            lhs = TVal::Bool(lhs.as_bool() && rhs.as_bool());
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self, depth: usize) -> Result<TVal, ()> {
        let mut lhs = self.parse_cmp(depth + 1)?;
        while let Some(op) = self.peek().cloned() {
            let eq = match op {
                Tok::EqEq => true,
                Tok::NotEq => false,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_cmp(depth + 1)?;
            let is_equal = match (&lhs, &rhs) {
                (TVal::Str(a), TVal::Str(b)) => a == b,
                (TVal::Bool(a), TVal::Bool(b)) => a == b,
                _ => match (lhs.as_num(), rhs.as_num()) {
                    (Some(a), Some(b)) => (a - b).abs() <= 1e-9,
                    _ => return Err(()),
                },
            };
            lhs = TVal::Bool(if eq { is_equal } else { !is_equal });
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self, depth: usize) -> Result<TVal, ()> {
        let mut lhs = self.parse_add(depth + 1)?;
        while let Some(op) = self.peek().cloned() {
            if !matches!(op, Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge) {
                break;
            }
            self.bump();
            let rhs = self.parse_add(depth + 1)?;
            let (Some(a), Some(b)) = (lhs.as_num(), rhs.as_num()) else {
                return Err(());
            };
            let res = match op {
                Tok::Lt => a < b,
                Tok::Le => a <= b,
                Tok::Gt => a > b,
                Tok::Ge => a >= b,
                _ => false,
            };
            lhs = TVal::Bool(res);
        }
        Ok(lhs)
    }

    fn parse_add(&mut self, depth: usize) -> Result<TVal, ()> {
        let mut lhs = self.parse_mul(depth + 1)?;
        while let Some(op) = self.peek().cloned() {
            let is_plus = match op {
                Tok::Plus => true,
                Tok::Minus => false,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_mul(depth + 1)?;
            let (Some(a), Some(b)) = (lhs.as_num(), rhs.as_num()) else {
                return Err(());
            };
            let out = if is_plus { a + b } else { a - b };
            if !out.is_finite() {
                return Err(());
            }
            lhs = TVal::Num(out);
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self, depth: usize) -> Result<TVal, ()> {
        let mut lhs = self.parse_unary(depth + 1)?;
        while let Some(op) = self.peek().cloned() {
            let is_mul = match op {
                Tok::Star => true,
                Tok::Slash => false,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary(depth + 1)?;
            let (Some(a), Some(b)) = (lhs.as_num(), rhs.as_num()) else {
                return Err(());
            };
            let out = if is_mul {
                a * b
            } else {
                if b.abs() <= 1e-12 {
                    return Err(());
                }
                a / b
            };
            if !out.is_finite() {
                return Err(());
            }
            lhs = TVal::Num(out);
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self, depth: usize) -> Result<TVal, ()> {
        if depth >= MAX_EXPR_DEPTH {
            return Err(());
        }
        if matches!(self.peek(), Some(Tok::Bang)) {
            self.bump();
            let inner = self.parse_unary(depth + 1)?;
            return Ok(TVal::Bool(!inner.as_bool()));
        }
        if matches!(self.peek(), Some(Tok::Minus)) {
            self.bump();
            let inner = self.parse_unary(depth + 1)?;
            let n = inner.as_num().ok_or(())?;
            return Ok(TVal::Num(-n));
        }
        self.parse_primary(depth + 1)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<TVal, ()> {
        if depth >= MAX_EXPR_DEPTH {
            return Err(());
        }
        match self.bump() {
            Some(Tok::Num(n)) => Ok(TVal::Num(*n)),
            Some(Tok::Str(s)) => Ok(TVal::Str(s.clone())),
            Some(Tok::LParen) => {
                let val = self.parse_or(depth + 1)?;
                match self.bump() {
                    Some(Tok::RParen) => Ok(val),
                    _ => Err(()),
                }
            }
            Some(Tok::Ident(name, idx_opt)) => {
                if idx_opt.is_none() && matches!(self.peek(), Some(Tok::LParen)) {
                    self.bump();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Tok::RParen)) {
                        loop {
                            args.push(self.parse_or(depth + 1)?);
                            if matches!(self.peek(), Some(Tok::Comma)) {
                                self.bump();
                            } else {
                                break;
                            }
                        }
                    }
                    if !matches!(self.bump(), Some(Tok::RParen)) {
                        return Err(());
                    }
                    return eval_builtin_fn(name, &args);
                }
                if idx_opt.is_none() {
                    if name == "true" {
                        return Ok(TVal::Bool(true));
                    }
                    if name == "false" {
                        return Ok(TVal::Bool(false));
                    }
                }
                lookup_indexed_var(name, *idx_opt, self.vars).ok_or(())
            }
            _ => Err(()),
        }
    }
}

fn eval_builtin_fn(name: &str, args: &[TVal]) -> Result<TVal, ()> {
    match name {
        "min" if args.len() == 2 => {
            let (Some(a), Some(b)) = (
                args.first().and_then(TVal::as_num),
                args.get(1).and_then(TVal::as_num),
            ) else {
                return Err(());
            };
            Ok(TVal::Num(a.min(b)))
        }
        "max" if args.len() == 2 => {
            let (Some(a), Some(b)) = (
                args.first().and_then(TVal::as_num),
                args.get(1).and_then(TVal::as_num),
            ) else {
                return Err(());
            };
            Ok(TVal::Num(a.max(b)))
        }
        "abs" if args.len() == 1 => {
            let a = args.first().and_then(TVal::as_num).ok_or(())?;
            Ok(TVal::Num(a.abs()))
        }
        "int" if args.len() == 1 => {
            let a = args.first().and_then(TVal::as_num).ok_or(())?;
            Ok(TVal::Num(a.trunc()))
        }
        "round" if args.len() == 1 => {
            let a = args.first().and_then(TVal::as_num).ok_or(())?;
            Ok(TVal::Num(a.round()))
        }
        _ => Err(()),
    }
}

fn eval_expr(expr_str: &str, vars: &TemplateVars) -> Result<TVal, ()> {
    let toks = tokenize_expr(expr_str)?;
    if toks.is_empty() {
        return Err(());
    }
    let mut parser = ExprParser {
        toks: &toks,
        pos: 0,
        vars,
    };
    let val = parser.parse_or(0)?;
    if parser.pos != toks.len() {
        return Err(());
    }
    Ok(val)
}
