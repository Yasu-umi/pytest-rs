r"""Evaluate match expressions, as used by `-k` and `-m` — upstream
_pytest/mark/expression.py port, plus the MarkMatcher/KeywordMatcher from
_pytest/mark/__init__.py and the engine glue.

The grammar is:

expression: expr? EOF
expr:       and_expr ('or' and_expr)*
and_expr:   not_expr ('and' not_expr)*
not_expr:   'not' not_expr | '(' expr ')' | ident kwargs?

ident:      (\w|:|\+|-|\.|\[|\]|\\|/)+
kwargs:     ('(' name '=' value ( ', ' name '=' value )*  ')')
name:       a valid ident, but not a reserved keyword
value:      (unescaped) string literal | (-)?[0-9]+ | 'False' | 'True' | 'None'

The semantics are:

- Empty expression evaluates to False.
- ident evaluates to True or False according to a provided matcher function.
- or/and/not evaluate according to the usual boolean semantics.
"""

import ast
import dataclasses
import enum
import keyword
import re
import types
from typing import Any

__all__ = ["Expression", "ExpressionMatcher"]

FILE_NAME = "<pytest match expression>"


class TokenType(enum.Enum):
    LPAREN = "left parenthesis"
    RPAREN = "right parenthesis"
    OR = "or"
    AND = "and"
    NOT = "not"
    IDENT = "identifier"
    EOF = "end of input"
    EQUAL = "="
    STRING = "string literal"
    COMMA = ","


@dataclasses.dataclass(frozen=True)
class Token:
    __slots__ = ("pos", "type", "value")
    type: TokenType
    value: str
    pos: int


class Scanner:
    __slots__ = ("current", "input", "tokens")

    def __init__(self, input: str) -> None:
        self.input = input
        self.tokens = self.lex(input)
        self.current = next(self.tokens)

    def lex(self, input):
        pos = 0
        while pos < len(input):
            if input[pos] in (" ", "\t"):
                pos += 1
            elif input[pos] == "(":
                yield Token(TokenType.LPAREN, "(", pos)
                pos += 1
            elif input[pos] == ")":
                yield Token(TokenType.RPAREN, ")", pos)
                pos += 1
            elif input[pos] == "=":
                yield Token(TokenType.EQUAL, "=", pos)
                pos += 1
            elif input[pos] == ",":
                yield Token(TokenType.COMMA, ",", pos)
                pos += 1
            elif (quote_char := input[pos]) in ("'", '"'):
                end_quote_pos = input.find(quote_char, pos + 1)
                if end_quote_pos == -1:
                    raise SyntaxError(
                        f'closing quote "{quote_char}" is missing',
                        (FILE_NAME, 1, pos + 1, input),
                    )
                value = input[pos : end_quote_pos + 1]
                if (backslash_pos := input.find("\\")) != -1:
                    raise SyntaxError(
                        r'escaping with "\" not supported in marker expression',
                        (FILE_NAME, 1, backslash_pos + 1, input),
                    )
                yield Token(TokenType.STRING, value, pos)
                pos += len(value)
            else:
                match = re.match(r"(:?\w|:|\+|-|\.|\[|\]|\\|/)+", input[pos:])
                if match:
                    value = match.group(0)
                    if value == "or":
                        yield Token(TokenType.OR, value, pos)
                    elif value == "and":
                        yield Token(TokenType.AND, value, pos)
                    elif value == "not":
                        yield Token(TokenType.NOT, value, pos)
                    else:
                        yield Token(TokenType.IDENT, value, pos)
                    pos += len(value)
                else:
                    raise SyntaxError(
                        f'unexpected character "{input[pos]}"',
                        (FILE_NAME, 1, pos + 1, input),
                    )
        yield Token(TokenType.EOF, "", pos)

    def accept(self, type, *, reject=False):
        if self.current.type is type:
            token = self.current
            if token.type is not TokenType.EOF:
                self.current = next(self.tokens)
            return token
        if reject:
            self.reject((type,))
        return None

    def reject(self, expected):
        raise SyntaxError(
            "expected {}; got {}".format(
                " OR ".join(type.value for type in expected),
                self.current.type.value,
            ),
            (FILE_NAME, 1, self.current.pos + 1, self.input),
        )


# True, False and None are legal match expression identifiers,
# but illegal as Python identifiers. To fix this, this prefix
# is added to identifiers in the conversion to Python AST.
IDENT_PREFIX = "$"


def expression(s: Scanner) -> ast.Expression:
    ret: ast.expr
    if s.accept(TokenType.EOF):
        ret = ast.Constant(False)
    else:
        ret = expr(s)
        s.accept(TokenType.EOF, reject=True)
    return ast.fix_missing_locations(ast.Expression(ret))


def expr(s: Scanner) -> ast.expr:
    ret = and_expr(s)
    while s.accept(TokenType.OR):
        rhs = and_expr(s)
        ret = ast.BoolOp(ast.Or(), [ret, rhs])
    return ret


def and_expr(s: Scanner) -> ast.expr:
    ret = not_expr(s)
    while s.accept(TokenType.AND):
        rhs = not_expr(s)
        ret = ast.BoolOp(ast.And(), [ret, rhs])
    return ret


def not_expr(s: Scanner) -> ast.expr:  # type: ignore[return]
    if s.accept(TokenType.NOT):
        return ast.UnaryOp(ast.Not(), not_expr(s))
    if s.accept(TokenType.LPAREN):
        ret = expr(s)
        s.accept(TokenType.RPAREN, reject=True)
        return ret
    ident = s.accept(TokenType.IDENT)
    if ident:
        name = ast.Name(IDENT_PREFIX + ident.value, ast.Load())
        if s.accept(TokenType.LPAREN):
            ret = ast.Call(func=name, args=[], keywords=all_kwargs(s))
            s.accept(TokenType.RPAREN, reject=True)
        else:
            ret = name
        return ret

    s.reject((TokenType.NOT, TokenType.LPAREN, TokenType.IDENT))


BUILTIN_MATCHERS = {"True": True, "False": False, "None": None}


def single_kwarg(s: Scanner) -> ast.keyword:
    keyword_name = s.accept(TokenType.IDENT, reject=True)
    if not keyword_name.value.isidentifier():
        raise SyntaxError(
            f"not a valid python identifier {keyword_name.value}",
            (FILE_NAME, 1, keyword_name.pos + 1, s.input),
        )
    if keyword.iskeyword(keyword_name.value):
        raise SyntaxError(
            f"unexpected reserved python keyword `{keyword_name.value}`",
            (FILE_NAME, 1, keyword_name.pos + 1, s.input),
        )
    s.accept(TokenType.EQUAL, reject=True)

    if value_token := s.accept(TokenType.STRING):
        value = value_token.value[1:-1]  # strip quotes
    else:
        value_token = s.accept(TokenType.IDENT, reject=True)
        if (number := value_token.value).isdigit() or (
            number.startswith("-") and number[1:].isdigit()
        ):
            value = int(number)
        elif value_token.value in BUILTIN_MATCHERS:
            value = BUILTIN_MATCHERS[value_token.value]
        else:
            raise SyntaxError(
                f'unexpected character/s "{value_token.value}"',
                (FILE_NAME, 1, value_token.pos + 1, s.input),
            )

    return ast.keyword(keyword_name.value, ast.Constant(value))


def all_kwargs(s: Scanner) -> list:
    ret = [single_kwarg(s)]
    while s.accept(TokenType.COMMA):
        ret.append(single_kwarg(s))
    return ret


class ExpressionMatcher:
    """Protocol stand-in: a callable (name, /, **kwargs) -> bool."""


@dataclasses.dataclass
class MatcherNameAdapter:
    matcher: Any
    name: str

    def __bool__(self) -> bool:
        return bool(self.matcher(self.name))

    def __call__(self, **kwargs) -> bool:
        return bool(self.matcher(self.name, **kwargs))


class MatcherAdapter:
    """Adapts a matcher function to a locals mapping as required by eval()."""

    def __init__(self, matcher) -> None:
        self.matcher = matcher

    def __getitem__(self, key: str) -> MatcherNameAdapter:
        return MatcherNameAdapter(matcher=self.matcher, name=key[len(IDENT_PREFIX) :])

    def __iter__(self):
        raise NotImplementedError()

    def __len__(self) -> int:
        raise NotImplementedError()


class Expression:
    """A compiled match expression as used by -k and -m.

    The expression can be evaluated against different matchers.
    """

    __slots__ = ("_code", "input")

    def __init__(self, input: str, code: types.CodeType) -> None:
        self.input = input
        self._code = code

    @classmethod
    def compile(cls, input: str) -> "Expression":
        """Compile a match expression.

        :raises SyntaxError: If the expression is malformed.
        """
        astexpr = expression(Scanner(input))
        code = compile(astexpr, filename=FILE_NAME, mode="eval")
        return Expression(input, code)

    def evaluate(self, matcher) -> bool:
        """Evaluate the match expression against a matcher callback."""
        return bool(eval(self._code, {"__builtins__": {}}, MatcherAdapter(matcher)))  # type: ignore[arg-type]


# ---- _pytest/mark/__init__.py matchers + engine glue -----------------------

NOT_SET = object()


@dataclasses.dataclass
class KeywordMatcher:
    """Given a list of names, matches any case-insensitive substring of one
    of these names (item/parent names, extra keywords, function-attribute
    names, mark names)."""

    __slots__ = ("_names",)

    _names: frozenset

    def __call__(self, subname, /, **kwargs):
        if kwargs:
            from pytest import UsageError

            raise UsageError("Keyword expressions do not support call parameters.")
        subname = subname.lower()
        return any(subname in name.lower() for name in self._names)


@dataclasses.dataclass
class MarkMatcher:
    """Matches marker names attached to the item; with kwargs, every given
    kwarg must equal the mark's."""

    __slots__ = ("own_mark_name_mapping",)

    own_mark_name_mapping: dict

    @classmethod
    def from_markers(cls, markers) -> "MarkMatcher":
        import collections

        mark_name_mapping = collections.defaultdict(list)
        for mark in markers:
            mark_name_mapping[mark.name].append(mark)
        return cls(mark_name_mapping)

    def __call__(self, name, /, **kwargs):
        if not (matches := self.own_mark_name_mapping.get(name, [])):
            return False

        for mark in matches:
            if all(mark.kwargs.get(k, NOT_SET) == v for k, v in kwargs.items()):
                return True
        return False


def compile_for_engine(expr: str, flag: str) -> Expression:
    """Compile, raising UsageError with upstream's _parse_expression wording."""
    try:
        return Expression.compile(expr)
    except SyntaxError as e:
        from pytest import UsageError

        raise UsageError(
            f"Wrong expression passed to '{flag}': {e.text}: at column {e.offset}: {e.msg}"
        ) from None


def evaluate_marks(compiled: Expression, marks) -> bool:
    return compiled.evaluate(MarkMatcher.from_markers(marks))


def evaluate_keywords(compiled: Expression, names) -> bool:
    return compiled.evaluate(KeywordMatcher(frozenset(names)))
