using System.Text.Json;

namespace Spotledger.Client.Workspace;

/// <summary>One changed field: its current value and the value it had when the draft began, both as the contract's JSON.</summary>
/// <param name="Name">The field's storage name.</param>
/// <param name="Json">The edited value.</param>
/// <param name="BaseJson">The value the document had when the edit began (to show what the server changed since).</param>
public sealed record FieldEdit(string Name, string Json, string BaseJson);

/// <summary>One row of a changed child table: its id (null for a new row) and its cell values.</summary>
/// <param name="Id">The server's row id, or null for a row the server has not named.</param>
/// <param name="Values">The cell values as the contract's JSON, by field name.</param>
public sealed record RowEdit(string? Id, IReadOnlyDictionary<string, string> Values);

/// <summary>A changed child table: the whole table as edited (the contract replaces a changed table in full).</summary>
/// <param name="Name">The table field's name.</param>
/// <param name="Rows">The rows in order.</param>
/// <param name="BaseHash">A hash of the table as the document had it when the edit began.</param>
public sealed record TableEdit(string Name, IReadOnlyList<RowEdit> Rows, string BaseHash);

/// <summary>
/// What is stored for one draft: the patch (changed fields and the child tables as edited), the
/// <see cref="BaseVersion"/> it was made against, a timestamp, the form's tab state and the title
/// the recovery list shows. It never holds a masked, hidden or secret field; the encrypted file is
/// the only place it lives.
/// </summary>
public sealed record DraftRecord
{
    /// <summary>The file format number; a newer file than this client understands is quarantined, never guessed at.</summary>
    public const int CurrentFormat = 1;

    /// <summary>The file format number.</summary>
    public int Format { get; init; } = CurrentFormat;

    /// <summary>Whose draft it is and which document.</summary>
    public required DraftKey Key { get; init; }

    /// <summary>The document version the edits are based on.</summary>
    public int BaseVersion { get; init; }

    /// <summary>When the draft was written.</summary>
    public DateTimeOffset SavedAtUtc { get; init; }

    /// <summary>The title the recovery list shows.</summary>
    public string? Title { get; init; }

    /// <summary>The form's tab state (the active tab and scroll position, as an opaque string the form reads back).</summary>
    public string? TabState { get; init; }

    /// <summary>The changed scalar fields.</summary>
    public IReadOnlyList<FieldEdit> Fields { get; init; } = [];

    /// <summary>The changed child tables.</summary>
    public IReadOnlyList<TableEdit> Tables { get; init; } = [];

    /// <summary>How many fields and tables the draft changes.</summary>
    public int ChangedCount => Fields.Count + Tables.Count;

    /// <summary>True when the draft changes nothing, so there is nothing to keep.</summary>
    public bool IsEmpty => ChangedCount == 0;
}

/// <summary>The stored form of a <see cref="DraftRecord"/>: plain data, so a corrupt or foreign file fails in one place.</summary>
internal static class DraftRecordJson
{
    private static readonly JsonSerializerOptions Options = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        WriteIndented = false,
    };

    public static byte[] Write(DraftRecord record)
    {
        var dto = new Dto
        {
            Format = record.Format,
            ServerId = record.Key.Scope.ServerId,
            UserId = record.Key.Scope.UserId,
            DocType = record.Key.DocType,
            DocumentId = record.Key.DocumentId,
            NewDraftId = record.Key.NewDraftId,
            BaseVersion = record.BaseVersion,
            SavedAtUtc = record.SavedAtUtc,
            Title = record.Title,
            TabState = record.TabState,
            Fields = [.. record.Fields],
            Tables = [.. record.Tables.Select(t => new TableDto
            {
                Name = t.Name,
                BaseHash = t.BaseHash,
                Rows = [.. t.Rows.Select(r => new RowDto { Id = r.Id, Values = new Dictionary<string, string>(r.Values) })],
            })],
        };
        return JsonSerializer.SerializeToUtf8Bytes(dto, Options);
    }

    /// <summary>Reads a record; throws <see cref="InvalidDataException"/> for anything that is not a well-formed current record.</summary>
    public static DraftRecord Read(ReadOnlySpan<byte> bytes)
    {
        Dto? dto;
        try
        {
            dto = JsonSerializer.Deserialize<Dto>(bytes, Options);
        }
        catch (JsonException ex)
        {
            throw new InvalidDataException("The draft is not valid JSON.", ex);
        }

        if (dto is null || dto.Format != DraftRecord.CurrentFormat
            || string.IsNullOrWhiteSpace(dto.ServerId) || string.IsNullOrWhiteSpace(dto.UserId) || string.IsNullOrWhiteSpace(dto.DocType)
            || (dto.DocumentId is null) == (dto.NewDraftId is null))
        {
            throw new InvalidDataException("The draft has an unknown format.");
        }

        return new DraftRecord
        {
            Format = dto.Format,
            Key = new DraftKey(new DraftScope(dto.ServerId, dto.UserId), dto.DocType, dto.DocumentId, dto.NewDraftId),
            BaseVersion = dto.BaseVersion,
            SavedAtUtc = dto.SavedAtUtc,
            Title = dto.Title,
            TabState = dto.TabState,
            Fields = [.. dto.Fields ?? []],
            Tables = [.. (dto.Tables ?? []).Select(t => new TableEdit(
                t.Name ?? throw new InvalidDataException("A table has no name."),
                [.. (t.Rows ?? []).Select(r => new RowEdit(r.Id, r.Values ?? new Dictionary<string, string>()))],
                t.BaseHash ?? string.Empty))],
        };
    }

    private sealed class Dto
    {
        public int Format { get; set; }
        public string? ServerId { get; set; }
        public string? UserId { get; set; }
        public string? DocType { get; set; }
        public string? DocumentId { get; set; }
        public string? NewDraftId { get; set; }
        public int BaseVersion { get; set; }
        public DateTimeOffset SavedAtUtc { get; set; }
        public string? Title { get; set; }
        public string? TabState { get; set; }
        public List<FieldEdit>? Fields { get; set; }
        public List<TableDto>? Tables { get; set; }
    }

    private sealed class TableDto
    {
        public string? Name { get; set; }
        public string? BaseHash { get; set; }
        public List<RowDto>? Rows { get; set; }
    }

    private sealed class RowDto
    {
        public string? Id { get; set; }
        public Dictionary<string, string>? Values { get; set; }
    }
}
