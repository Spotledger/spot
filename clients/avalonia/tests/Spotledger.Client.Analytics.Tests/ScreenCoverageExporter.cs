using System.Reflection;
using System.Text.Json;

namespace Spotledger.Client.Analytics.Tests;

/// <summary>
/// Reflection-based counterpart of the static scan in tools/ui-coverage/check.py: lists the
/// screens ([ScreenId] on types) and the test coverage ([CoversScreen] on test types and
/// methods) found in a set of assemblies, as the screens-found.json the checker merges with
/// --found. Test code, not shipped: it uses reflection freely.
/// </summary>
internal static class ScreenCoverageExporter
{
    public static string Export(IEnumerable<Assembly> assemblies)
    {
        var implemented = new SortedDictionary<string, string>(StringComparer.Ordinal);
        var covered = new SortedDictionary<string, SortedSet<string>>(StringComparer.Ordinal);

        foreach (var assembly in assemblies)
        {
            foreach (var type in SafeTypes(assembly))
            {
                foreach (var a in type.GetCustomAttributes<ScreenIdAttribute>(false))
                {
                    implemented[a.Id] = type.FullName ?? type.Name;
                }

                AddCovers(type.GetCustomAttributes<CoversScreenAttribute>(false), covered);
                foreach (var m in type.GetMethods(BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Instance | BindingFlags.Static | BindingFlags.DeclaredOnly))
                {
                    AddCovers(m.GetCustomAttributes<CoversScreenAttribute>(false), covered);
                }
            }
        }

        using var stream = new MemoryStream();
        using (var w = new Utf8JsonWriter(stream, new JsonWriterOptions { Indented = true }))
        {
            w.WriteStartObject();
            w.WriteStartObject("implemented");
            foreach (var (id, where) in implemented)
            {
                w.WriteString(id, where);
            }

            w.WriteEndObject();
            w.WriteStartObject("covered");
            foreach (var (id, states) in covered)
            {
                w.WriteStartObject(id);
                w.WriteStartArray("states");
                foreach (var s in states)
                {
                    w.WriteStringValue(s);
                }

                w.WriteEndArray();
                w.WriteEndObject();
            }

            w.WriteEndObject();
            w.WriteEndObject();
        }

        return System.Text.Encoding.UTF8.GetString(stream.ToArray()) + "\n";
    }

    private static void AddCovers(IEnumerable<CoversScreenAttribute> attrs, SortedDictionary<string, SortedSet<string>> covered)
    {
        foreach (var a in attrs)
        {
            if (!covered.TryGetValue(a.Id, out var set))
            {
                covered[a.Id] = set = new SortedSet<string>(StringComparer.Ordinal);
            }

            if (a.States.Count == 0)
            {
                set.Add("default");
            }

            foreach (var s in a.States)
            {
                set.Add(s);
            }
        }
    }

    private static IEnumerable<Type> SafeTypes(Assembly assembly)
    {
        try
        {
            return assembly.GetTypes();
        }
        catch (ReflectionTypeLoadException e)
        {
            return e.Types.OfType<Type>();
        }
    }
}
