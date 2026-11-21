using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace Spotledger.Client.Expressions;

internal enum TokKind
{
    Int,
    Dec,
    Text,
    Ident,
    And,
    Or,
    Not,
    In,
    True,
    False,
    Null,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Question,
    Colon,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    End,
}

internal readonly struct Token
{
    internal Token(TokKind kind, long intValue, ExpressionDecimal decValue, string textValue, int offset)
    {
        Kind = kind;
        IntValue = intValue;
        DecValue = decValue;
        TextValue = textValue;
        Offset = offset;
    }

    internal TokKind Kind { get; }
    internal long IntValue { get; }
    internal ExpressionDecimal DecValue { get; }
    internal string TextValue { get; }
    /// <summary>The 0-based Unicode code point offset of the token's first character.</summary>
    internal int Offset { get; }
}

/// <summary>
/// The lexer (specification section 2). Error offsets count Unicode scalar values (code
/// points): the lexer converts UTF-16 indices to code point offsets with
/// <see cref="CodePointOffset" />. Numbers and identifiers are ASCII, so a surrogate pair can
/// only precede them or appear inside a text literal; text literals carry their chars over.
/// </summary>
internal static class ExpressionLexer
{
    internal static List<Token> Lex(string source)
    {
        List<Token> tokens = new();
        int i = 0;
        while (i < source.Length)
        {
            char c = source[i];
            if (c == ' ' || c == '\t' || c == '\n' || c == '\r')
            {
                i += 1;
                continue;
            }
            int start = CodePointOffset(source, i);
            if (char.IsAsciiDigit(c))
            {
                i = ScanNumber(source, i, start, tokens);
            }
            else if (char.IsAsciiLetter(c) || c == '_')
            {
                i = ScanWord(source, i, start, tokens);
            }
            else if (c == '\'' || c == '"')
            {
                i = ScanText(source, i, start, tokens);
            }
            else
            {
                i = ScanPunctuation(source, i, start, tokens);
            }
        }
        tokens.Add(new Token(TokKind.End, 0, default, string.Empty, CodePointOffset(source, source.Length)));
        return tokens;
    }

    /// <summary>The 0-based code point count before UTF-16 index <paramref name="index" />.</summary>
    private static int CodePointOffset(string s, int index)
    {
        int offset = 0;
        for (int at = 0; at < index; at++)
        {
            offset += char.IsSurrogate(s[at]) && char.IsHighSurrogate(s[at]) && at + 1 < s.Length && char.IsLowSurrogate(s[at + 1]) ? 1 : char.IsSurrogate(s[at]) ? 0 : 1;
        }
        return offset;
    }

    private static int ScanNumber(string source, int i, int start, List<Token> tokens)
    {
        int intStart = i;
        while (i < source.Length && char.IsAsciiDigit(source[i]))
        {
            i += 1;
        }
        int intEnd = i;
        bool hasFraction = i + 1 < source.Length
            && source[i] == '.'
            && char.IsAsciiDigit(source[i + 1]);
        if (hasFraction)
        {
            i += 1;
            int fracStart = i;
            while (i < source.Length && char.IsAsciiDigit(source[i]))
            {
                i += 1;
            }
            string intPart = source[intStart..intEnd];
            string fracPart = source[fracStart..i];
            ExpressionDecimal? d = ExpressionDecimal.MakeFromDigits(intPart.AsSpan(), fracPart.AsSpan());
            if (d is null)
            {
                throw ParseError(ExpressionErrorCode.NumberRange, "a decimal literal has at most 28 significant digits and scale 28", start);
            }
            tokens.Add(new Token(TokKind.Dec, 0, d.Value, string.Empty, start));
            return i;
        }
        string digits = source[intStart..intEnd];
        if (!long.TryParse(digits, NumberStyles.None, CultureInfo.InvariantCulture, out long n))
        {
            throw ParseError(ExpressionErrorCode.NumberRange, "an integer literal must fit in 64 bits", start);
        }
        tokens.Add(new Token(TokKind.Int, n, default, string.Empty, start));
        return i;
    }

    private static int ScanWord(string source, int i, int start, List<Token> tokens)
    {
        int end = i;
        while (end < source.Length && (char.IsAsciiLetter(source[end]) || char.IsAsciiDigit(source[end]) || source[end] == '_'))
        {
            end += 1;
        }
        string word = source[i..end];
        TokKind kind = word switch
        {
            "and" => TokKind.And,
            "or" => TokKind.Or,
            "not" => TokKind.Not,
            "in" => TokKind.In,
            "true" => TokKind.True,
            "false" => TokKind.False,
            "null" => TokKind.Null,
            _ => TokKind.Ident,
        };
        tokens.Add(new Token(kind, 0, default, word, start));
        return end;
    }

    private static int ScanText(string source, int i, int start, List<Token> tokens)
    {
        char quote = source[i];
        i += 1;
        StringBuilder text = new();
        while (true)
        {
            if (i >= source.Length)
            {
                throw ParseError(ExpressionErrorCode.UnterminatedString, "a text literal is not closed", start);
            }
            char ch = source[i];
            i += 1;
            if (ch == quote)
            {
                break;
            }
            if (ch == '\\')
            {
                if (i >= source.Length)
                {
                    throw ParseError(ExpressionErrorCode.UnterminatedString, "a text literal is not closed", start);
                }
                char esc = source[i];
                char decoded;
                switch (esc)
                {
                    case '\\': decoded = '\\'; break;
                    case '\'': decoded = '\''; break;
                    case '"': decoded = '"'; break;
                    case 'n': decoded = '\n'; break;
                    case 't': decoded = '\t'; break;
                    case 'r': decoded = '\r'; break;
                    default:
                        throw ParseError(ExpressionErrorCode.BadEscape, "unknown escape in a text literal", CodePointOffset(source, i - 1));
                }
                i += 1;
                text.Append(decoded);
            }
            else
            {
                text.Append(ch);
            }
        }
        tokens.Add(new Token(TokKind.Text, 0, default, text.ToString(), start));
        return i;
    }

    private static int ScanPunctuation(string source, int i, int start, List<Token> tokens)
    {
        char c = source[i];
        int endIndex = i + 1;
        char? next = endIndex < source.Length ? source[endIndex] : null;
        switch (c)
        {
            case '(': tokens.Add(new Token(TokKind.LParen, 0, default, string.Empty, start)); break;
            case ')': tokens.Add(new Token(TokKind.RParen, 0, default, string.Empty, start)); break;
            case '[': tokens.Add(new Token(TokKind.LBracket, 0, default, string.Empty, start)); break;
            case ']': tokens.Add(new Token(TokKind.RBracket, 0, default, string.Empty, start)); break;
            case ',': tokens.Add(new Token(TokKind.Comma, 0, default, string.Empty, start)); break;
            case '.': tokens.Add(new Token(TokKind.Dot, 0, default, string.Empty, start)); break;
            case '?': tokens.Add(new Token(TokKind.Question, 0, default, string.Empty, start)); break;
            case ':': tokens.Add(new Token(TokKind.Colon, 0, default, string.Empty, start)); break;
            case '+': tokens.Add(new Token(TokKind.Plus, 0, default, string.Empty, start)); break;
            case '-': tokens.Add(new Token(TokKind.Minus, 0, default, string.Empty, start)); break;
            case '*': tokens.Add(new Token(TokKind.Star, 0, default, string.Empty, start)); break;
            case '/': tokens.Add(new Token(TokKind.Slash, 0, default, string.Empty, start)); break;
            case '%': tokens.Add(new Token(TokKind.Percent, 0, default, string.Empty, start)); break;
            case '=' or '!':
                if (next != '=')
                {
                    throw ParseError(ExpressionErrorCode.Syntax, "unexpected character `{c}`", start);
                }
                tokens.Add(new Token(c == '=' ? TokKind.EqEq : TokKind.NotEq, 0, default, string.Empty, start));
                endIndex += 1;
                break;
            case '<' or '>':
                if (next == '=')
                {
                    tokens.Add(new Token(c == '<' ? TokKind.Le : TokKind.Ge, 0, default, string.Empty, start));
                    endIndex += 1;
                }
                else
                {
                    tokens.Add(new Token(c == '<' ? TokKind.Lt : TokKind.Gt, 0, default, string.Empty, start));
                }
                break;
            default:
                throw ParseError(
                    ExpressionErrorCode.Syntax,
                    $"unexpected character `{DescribeUnexpected(c)}`",
                    start);
        }
        return endIndex;
    }

    private static string DescribeUnexpected(char c) => c.ToString();

    /// <summary>The token's spelling, for messages (mirrors the Rust `describe`).</summary>
    internal static string Describe(Token t) => t.Kind switch
    {
        TokKind.Int => t.IntValue.ToString(provider: CultureInfo.InvariantCulture),
        TokKind.Dec => t.DecValue.ToString(),
        TokKind.Text => "text",
        TokKind.Ident => t.TextValue,
        TokKind.And => "and",
        TokKind.Or => "or",
        TokKind.Not => "not",
        TokKind.In => "in",
        TokKind.True => "true",
        TokKind.False => "false",
        TokKind.Null => "null",
        TokKind.LParen => "(",
        TokKind.RParen => ")",
        TokKind.LBracket => "[",
        TokKind.RBracket => "]",
        TokKind.Comma => ",",
        TokKind.Dot => ".",
        TokKind.Question => "?",
        TokKind.Colon => ":",
        TokKind.Plus => "+",
        TokKind.Minus => "-",
        TokKind.Star => "*",
        TokKind.Slash => "/",
        TokKind.Percent => "%",
        TokKind.EqEq => "==",
        TokKind.NotEq => "!=",
        TokKind.Lt => "<",
        TokKind.Le => "<=",
        TokKind.Gt => ">",
        TokKind.Ge => ">=",
        _ => "end",
    };

    /// <summary>Builds the parse exception in (code, message, offset) order.</summary>
    private static ExpressionParseException ParseError(string code, string message, int offset) =>
        new(code, message, offset);
}
