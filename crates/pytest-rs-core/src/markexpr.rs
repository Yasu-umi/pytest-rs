//! -k / -m selection expressions: a tiny and/or/not/parens evaluator over
//! ident predicates (no eval, matching pytest's keyword/mark expressions).

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    And,
    Or,
    Not,
    Open,
    Close,
}

fn tokenize(expr: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, tokens: &mut Vec<Token>| {
        if current.is_empty() {
            return;
        }
        let token = match current.as_str() {
            "and" => Token::And,
            "or" => Token::Or,
            "not" => Token::Not,
            ident => Token::Ident(ident.to_string()),
        };
        tokens.push(token);
        current.clear();
    };
    for ch in expr.chars() {
        match ch {
            '(' => {
                flush(&mut current, &mut tokens);
                tokens.push(Token::Open);
            }
            ')' => {
                flush(&mut current, &mut tokens);
                tokens.push(Token::Close);
            }
            ch if ch.is_whitespace() => flush(&mut current, &mut tokens),
            ch => current.push(ch),
        }
    }
    flush(&mut current, &mut tokens);
    tokens
}

struct Parser<'a, F: Fn(&str) -> bool> {
    tokens: &'a [Token],
    pos: usize,
    matches: &'a F,
}

impl<F: Fn(&str) -> bool> Parser<'_, F> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn or_expr(&mut self) -> Result<bool, String> {
        let mut value = self.and_expr()?;
        while self.peek() == Some(&Token::Or) {
            self.pos += 1;
            let rhs = self.and_expr()?;
            value = value || rhs;
        }
        Ok(value)
    }

    fn and_expr(&mut self) -> Result<bool, String> {
        let mut value = self.not_expr()?;
        while self.peek() == Some(&Token::And) {
            self.pos += 1;
            let rhs = self.not_expr()?;
            value = value && rhs;
        }
        Ok(value)
    }

    fn not_expr(&mut self) -> Result<bool, String> {
        if self.peek() == Some(&Token::Not) {
            self.pos += 1;
            return Ok(!self.not_expr()?);
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<bool, String> {
        match self.peek().cloned() {
            Some(Token::Open) => {
                self.pos += 1;
                let value = self.or_expr()?;
                if self.peek() != Some(&Token::Close) {
                    return Err("expected )".to_string());
                }
                self.pos += 1;
                Ok(value)
            }
            Some(Token::Ident(ident)) => {
                self.pos += 1;
                Ok((self.matches)(&ident))
            }
            other => Err(format!("unexpected token: {other:?}")),
        }
    }
}

/// Evaluate a selection expression; `matches` decides one identifier.
pub fn evaluate(expr: &str, matches: impl Fn(&str) -> bool) -> Result<bool, String> {
    let tokens = tokenize(expr);
    if tokens.is_empty() {
        return Ok(true);
    }
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        matches: &matches,
    };
    let value = parser.or_expr()?;
    if parser.pos != tokens.len() {
        return Err("unexpected trailing tokens".to_string());
    }
    Ok(value)
}
