using System.Globalization;
using System.Text;

namespace Spotledger.Client.Effects.Tests.Toml;

/// <summary>
/// A reader for the small TOML subset the effects conformance corpus uses: table headers,
/// array-of-table headers, bare keys, basic and multi-line literal strings, integers,
/// booleans, arrays (multi-line, trailing commas allowed) and inline tables. It exists so the
/// C# client reads the corpus the Rust crate owns without adding a TOML dependency.
/// </summary>
/// <remarks>
/// A table is a <see cref="Dictionary{TKey,TValue}"/> of string to node, an array a
/// <see cref="List{T}"/> of nodes, a scalar a <see cref="string"/>, <see cref="long"/> or
/// <see cref="bool"/>. Anything the subset does not define is an error, never a silent guess:
/// the corpus is checked in, so a parse failure is a bug worth seeing.
/// </remarks>
internal static class TomlParser
{
    /// <summary>Parses <paramref name="text"/> into its root table.</summary>
    public static Dictionary<string, object?> Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        var scanner = new Scanner(text);
        var root = new Dictionary<string, object?>(StringComparer.Ordinal);
        Dictionary<string, object?> current = root;
        while (true)
        {
            scanner.SkipBlank();
            if (scanner.AtEnd)
            {
                return root;
            }

            if (scanner.Peek() == '[')
            {
                bool arrayElement = scanner.PeekAt(1) == '[';
                scanner.Advance(arrayElement ? 2 : 1);
                string header = scanner.ReadUntil(']', "a table header");
                if (arrayElement)
                {
                    scanner.Expect(']');
                }

                string[] path = SplitPath(header);
                current = arrayElement ? AppendTableArray(root, path) : OpenTable(root, path);
                continue;
            }

            string key = scanner.ReadKey();
            scanner.Expect('=');
            current[key] = scanner.ReadValue();
            scanner.SkipToStatementEnd();
        }
    }

    private static string[] SplitPath(string header)
    {
        string trimmed = header.Trim();
        if (trimmed.Length == 0)
        {
            throw new FormatException("a table header names a table");
        }

        string[] path = trimmed.Split('.');
        foreach (string segment in path)
        {
            if (segment.Length == 0)
            {
                throw new FormatException(string.Create(
                    CultureInfo.InvariantCulture, $"`{header}` is not a dotted table path"));
            }
        }

        return path;
    }

    private static Dictionary<string, object?> OpenTable(Dictionary<string, object?> root, string[] path)
    {
        Dictionary<string, object?> parent = Descend(root, path, path.Length - 1);
        string name = path[^1];
        if (!parent.TryGetValue(name, out object? existing))
        {
            var table = new Dictionary<string, object?>(StringComparer.Ordinal);
            parent[name] = table;
            return table;
        }

        return existing as Dictionary<string, object?>
            ?? throw new FormatException(string.Create(
                CultureInfo.InvariantCulture, $"`{string.Join('.', path)}` is already a value, not a table"));
    }

    private static Dictionary<string, object?> AppendTableArray(Dictionary<string, object?> root, string[] path)
    {
        Dictionary<string, object?> parent = Descend(root, path, path.Length - 1);
        string name = path[^1];
        if (!parent.TryGetValue(name, out object? existing))
        {
            var list = new List<object?>();
            parent[name] = list;
            var table = new Dictionary<string, object?>(StringComparer.Ordinal);
            list.Add(table);
            return table;
        }

        if (existing is not List<object?> elements)
        {
            throw new FormatException(string.Create(
                CultureInfo.InvariantCulture, $"`{string.Join('.', path)}` is already a value, not a table array"));
        }

        var appended = new Dictionary<string, object?>(StringComparer.Ordinal);
        elements.Add(appended);
        return appended;
    }

    /// <summary>Walks the first <paramref name="count"/> segments, entering the last element of
    /// a table array when the segment names one (a nested table of an array element).</summary>
    private static Dictionary<string, object?> Descend(Dictionary<string, object?> root, string[] path, int count)
    {
        Dictionary<string, object?> current = root;
        for (int i = 0; i < count; i++)
        {
            string segment = path[i];
            if (!current.TryGetValue(segment, out object? existing))
            {
                var created = new Dictionary<string, object?>(StringComparer.Ordinal);
                current[segment] = created;
                current = created;
                continue;
            }

            current = existing switch
            {
                Dictionary<string, object?> table => table,
                List<object?> { Count: > 0 } elements when elements[^1] is Dictionary<string, object?> last => last,
                _ => throw new FormatException(string.Create(
                    CultureInfo.InvariantCulture, $"`{segment}` is not a table")),
            };
        }

        return current;
    }

    /// <summary>The character scanner under the parser.</summary>
    private sealed class Scanner
    {
        private readonly string _text;
        private int _index;

        public Scanner(string text) => _text = text;

        public bool AtEnd => _index >= _text.Length;

        public char Peek() => _text[_index];

        public char PeekAt(int offset) => _index + offset < _text.Length ? _text[_index + offset] : '\0';

        public void Advance(int count) => _index += count;

        public void Expect(char expected)
        {
            if (AtEnd || _text[_index] != expected)
            {
                throw new FormatException(string.Create(
                    CultureInfo.InvariantCulture,
                    $"expected `{expected}` at offset {_index}"));
            }

            _index++;
        }

        /// <summary>Skips whitespace, newlines and comments.</summary>
        public void SkipBlank()
        {
            while (!AtEnd)
            {
                char c = _text[_index];
                if (c is ' ' or '\t' or '\r' or '\n')
                {
                    _index++;
                    continue;
                }

                if (c == '#')
                {
                    while (!AtEnd && _text[_index] != '\n')
                    {
                        _index++;
                    }

                    continue;
                }

                return;
            }
        }

        /// <summary>After a value: skip spaces and an end-of-line comment.</summary>
        public void SkipToStatementEnd()
        {
            while (!AtEnd && _text[_index] is ' ' or '\t' or '\r')
            {
                _index++;
            }

            if (!AtEnd && _text[_index] == '#')
            {
                while (!AtEnd && _text[_index] != '\n')
                {
                    _index++;
                }
            }
        }

        /// <summary>Reads up to <paramref name="terminator"/> (exclusive), consuming it.</summary>
        public string ReadUntil(char terminator, string what)
        {
            int start = _index;
            while (!AtEnd && _text[_index] != terminator)
            {
                if (_text[_index] == '\n')
                {
                    throw new FormatException($"{what} ends before the line does (offset {start})");
                }

                _index++;
            }

            if (AtEnd)
            {
                throw new FormatException($"{what} is not closed (offset {start})");
            }

            string value = _text[start.._index];
            _index++;
            return value;
        }

        public string ReadKey()
        {
            SkipInlineSpace();
            int start = _index;
            while (!AtEnd && _text[_index] is not ('=' or '\n'))
            {
                _index++;
            }

            if (AtEnd || _text[_index] != '=')
            {
                throw new FormatException(string.Create(
                    CultureInfo.InvariantCulture, $"expected a key at offset {start}"));
            }

            string key = Unquote(_text[start.._index].Trim());
            if (key.Length == 0)
            {
                throw new FormatException(string.Create(
                    CultureInfo.InvariantCulture, $"expected a key at offset {start}"));
            }

            return key;
        }

        /// <summary>A quoted key (<c>"items[1].line_total"</c>) keeps its brackets; the quotes go.</summary>
        private static string Unquote(string key)
        {
            if (key.Length < 2 || key[0] != '"' || key[^1] != '"')
            {
                return key;
            }

            var builder = new StringBuilder(key.Length - 2);
            for (int i = 1; i < key.Length - 1; i++)
            {
                if (key[i] == '\\' && i + 1 < key.Length - 1)
                {
                    i++;
                    builder.Append(key[i] switch
                    {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        _ => key[i],
                    });
                    continue;
                }

                builder.Append(key[i]);
            }

            return builder.ToString();
        }

        /// <summary>Reads one value, leaving the index after it.</summary>
        public object? ReadValue()
        {
            SkipInlineSpace();
            if (AtEnd)
            {
                throw new FormatException("a value is missing");
            }

            if (PeekAt(0) == '\'' && PeekAt(1) == '\'' && PeekAt(2) == '\'')
            {
                return ReadMultilineLiteral();
            }

            return Peek() switch
            {
                '"' => ReadBasicString(),
                '[' => ReadArray(),
                '{' => ReadInlineTable(),
                _ => ReadScalar(),
            };
        }

        private void SkipInlineSpace()
        {
            while (!AtEnd && _text[_index] is ' ' or '\t')
            {
                _index++;
            }
        }

        private void SkipSpaceAndNewlines()
        {
            while (!AtEnd)
            {
                char c = _text[_index];
                if (c is ' ' or '\t' or '\r' or '\n')
                {
                    _index++;
                    continue;
                }

                if (c == '#')
                {
                    while (!AtEnd && _text[_index] != '\n')
                    {
                        _index++;
                    }

                    continue;
                }

                return;
            }
        }

        private string ReadMultilineLiteral()
        {
            Advance(3);
            // A newline directly after the opening delimiter is not part of the value.
            if (!AtEnd && _text[_index] == '\r')
            {
                _index++;
            }

            if (!AtEnd && _text[_index] == '\n')
            {
                _index++;
            }

            int start = _index;
            while (!AtEnd)
            {
                if (PeekAt(0) == '\'' && PeekAt(1) == '\'' && PeekAt(2) == '\'')
                {
                    string value = _text[start.._index];
                    Advance(3);
                    return value;
                }

                _index++;
            }

            throw new FormatException("a multi-line literal string is not closed");
        }

        private string ReadBasicString()
        {
            Expect('"');
            var builder = new StringBuilder();
            while (!AtEnd)
            {
                char c = _text[_index];
                if (c == '"')
                {
                    _index++;
                    return builder.ToString();
                }

                if (c == '\\')
                {
                    _index++;
                    if (AtEnd)
                    {
                        break;
                    }

                    char escape = _text[_index++];
                    switch (escape)
                    {
                        case 'n':
                            builder.Append('\n');
                            break;
                        case 't':
                            builder.Append('\t');
                            break;
                        case 'r':
                            builder.Append('\r');
                            break;
                        case '"':
                            builder.Append('"');
                            break;
                        case '\\':
                            builder.Append('\\');
                            break;
                        case 'u':
                            string hex = _text.Substring(_index, 4);
                            _index += 4;
                            builder.Append((char)int.Parse(hex, NumberStyles.HexNumber, CultureInfo.InvariantCulture));
                            break;
                        default:
                            builder.Append(escape);
                            break;
                    }

                    continue;
                }

                builder.Append(c);
                _index++;
            }

            throw new FormatException("a basic string is not closed");
        }

        private List<object?> ReadArray()
        {
            Expect('[');
            var items = new List<object?>();
            while (true)
            {
                SkipSpaceAndNewlines();
                if (AtEnd)
                {
                    throw new FormatException("an array is not closed");
                }

                if (Peek() == ']')
                {
                    _index++;
                    return items;
                }

                items.Add(ReadValue());
                SkipSpaceAndNewlines();
                if (!AtEnd && Peek() == ',')
                {
                    _index++;
                    continue;
                }

                if (!AtEnd && Peek() == ']')
                {
                    _index++;
                    return items;
                }

                throw new FormatException("an array element is not followed by `,` or `]`");
            }
        }

        private Dictionary<string, object?> ReadInlineTable()
        {
            Expect('{');
            var table = new Dictionary<string, object?>(StringComparer.Ordinal);
            while (true)
            {
                SkipInlineSpace();
                if (!AtEnd && Peek() == '}')
                {
                    _index++;
                    return table;
                }

                string key = ReadKey();
                Expect('=');
                table[key] = ReadValue();
                SkipInlineSpace();
                if (!AtEnd && Peek() == ',')
                {
                    _index++;
                    continue;
                }

                if (!AtEnd && Peek() == '}')
                {
                    _index++;
                    return table;
                }

                throw new FormatException("an inline table member is not followed by `,` or `}`");
            }
        }

        private object ReadScalar()
        {
            int start = _index;
            while (!AtEnd && _text[_index] is not (' ' or '\t' or '\r' or '\n' or ',' or ']' or '}' or '#'))
            {
                _index++;
            }

            string text = _text[start.._index];
            if (text == "true")
            {
                return true;
            }

            if (text == "false")
            {
                return false;
            }

            if (long.TryParse(text, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out long number))
            {
                return number;
            }

            throw new FormatException(string.Create(
                CultureInfo.InvariantCulture, $"`{text}` is not a scalar this reader supports"));
        }
    }
}
