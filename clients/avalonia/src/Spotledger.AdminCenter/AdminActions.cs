using System.Security.Cryptography;
using System.Text;

namespace Spotledger.AdminCenter;

/// <summary>An argument of an operation as the catalogue declares it.</summary>
public sealed record ArgumentSpec(string Name, bool IsSecret = false);

/// <summary>One catalogue operation: its id, arguments and how many signatures it needs.</summary>
public sealed record OperationSpec(
    string OperationId,
    IReadOnlyList<ArgumentSpec> Arguments,
    int RequiredApprovals = 0);

/// <summary>
/// An action the console performs: an operation id and its arguments. The same record
/// renders the CLI command, the signed-command preview and executes the call (one code path).
/// </summary>
public sealed record AdminAction(string OperationId, IReadOnlyList<KeyValuePair<string, string>> Args)
{
    public static AdminAction Create(string operationId, params (string Name, string Value)[] args) =>
        new(operationId, [.. args.Select(a => new KeyValuePair<string, string>(a.Name, a.Value))]);

    /// <summary>Argument values may be secrets: only the operation id and argument names are printed.</summary>
    public override string ToString() =>
        $"AdminAction {{ {OperationId}, args: {string.Join(",", Args.Select(a => a.Key))} }}";
}

public interface IAdminExecutor
{
    Task ExecuteAsync(AdminAction action, CancellationToken cancellationToken);
}

/// <summary>Renders the exact <c>spot</c> command and the canonical payload that gets signed.</summary>
public sealed class EquivalentCommand
{
    private const string Masked = "'***'";

    private readonly Dictionary<string, OperationSpec> _catalogue;

    public EquivalentCommand(IEnumerable<OperationSpec> catalogue)
    {
        ArgumentNullException.ThrowIfNull(catalogue);
        _catalogue = catalogue.ToDictionary(spec => spec.OperationId, StringComparer.Ordinal);
    }

    public OperationSpec Spec(string operationId) =>
        _catalogue.TryGetValue(operationId, out var spec)
            ? spec
            : throw new KeyNotFoundException($"Operation '{operationId}' is not in the catalogue.");

    /// <summary>The CLI line. Secret arguments are masked: a secret is never rendered or copied.</summary>
    public string Cli(AdminAction action)
    {
        ArgumentNullException.ThrowIfNull(action);
        var spec = Spec(action.OperationId);
        var builder = new StringBuilder("spot admin ").Append(action.OperationId);
        foreach (var (name, value) in action.Args)
        {
            var secret = spec.Arguments.Any(a => a.IsSecret && string.Equals(a.Name, name, StringComparison.Ordinal));
            builder.Append(" --").Append(name).Append(' ').Append(secret ? Masked : Quote(value));
        }

        return builder.ToString();
    }

    /// <summary>The canonical text a signer signs: operation id and arguments sorted by name, secrets masked.</summary>
    public string SignedCommandPreview(AdminAction action)
    {
        ArgumentNullException.ThrowIfNull(action);
        var spec = Spec(action.OperationId);
        var builder = new StringBuilder("op=").Append(action.OperationId).Append('\n');
        foreach (var (name, value) in action.Args.OrderBy(a => a.Key, StringComparer.Ordinal))
        {
            var secret = spec.Arguments.Any(a => a.IsSecret && string.Equals(a.Name, name, StringComparison.Ordinal));
            builder.Append(name).Append('=').Append(secret ? "***" : value).Append('\n');
        }

        builder.Append("sha256=").Append(Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(builder.ToString()))));
        return builder.ToString();
    }

    private static string Quote(string value)
    {
        if (value.Length > 0 && value.All(c => char.IsAsciiLetterOrDigit(c) || c is '-' or '_' or '.' or '/' or ':' or '='))
        {
            return value;
        }

        return "'" + value.Replace("'", "'\\''", StringComparison.Ordinal) + "'";
    }
}
