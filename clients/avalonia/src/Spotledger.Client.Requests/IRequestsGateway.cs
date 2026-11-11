namespace Spotledger.Client.Requests;

/// <summary>
/// The screens' seam to the server. Every method throws <see cref="Spotledger.Client.Api.ApiRequestException"/>
/// for a contract error; none decides a permission. A host supplies the production implementation
/// over the platform's request endpoints; tests supply a fake.
/// </summary>
public interface IRequestsGateway
{
    /// <summary>How <paramref name="recordType"/> may be created.</summary>
    Task<EntryInfo> GetEntryInfoAsync(string recordType, CancellationToken cancellationToken);

    /// <summary>Starts the entry process as the signed-in person.</summary>
    Task<StartedRequest> StartAsync(string processId, CancellationToken cancellationToken);

    /// <summary>The form step behind <paramref name="taskId"/>.</summary>
    Task<StepInfo> GetStepAsync(string taskId, CancellationToken cancellationToken);

    /// <summary>Completes the form step; the server validates the draft and keeps the step open on failure.</summary>
    Task CompleteStepAsync(string taskId, CancellationToken cancellationToken);

    /// <summary>The runs of the signed-in person in one view.</summary>
    Task<IReadOnlyList<RequestSummary>> ListMineAsync(RequestFilter filter, CancellationToken cancellationToken);

    /// <summary>One run with its timeline, records, tasks, comments and conversations.</summary>
    Task<RequestDetail> GetRequestAsync(string requestId, CancellationToken cancellationToken);

    /// <summary>Undoes the run.</summary>
    Task UndoAsync(string requestId, CancellationToken cancellationToken);

    /// <summary>Cancels the run, with a reason when the person gave one.</summary>
    Task CancelAsync(string requestId, string? reason, CancellationToken cancellationToken);
}
