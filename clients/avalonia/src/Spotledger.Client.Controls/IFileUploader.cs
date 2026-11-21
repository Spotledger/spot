using System.IO;

namespace Spotledger.Client.Controls;

/// <summary>Transient upload input. The caller owns and disposes the stream after awaiting upload.</summary>
public sealed record UploadFile(string Name, string ContentType, long? Size, Stream Content);

/// <summary>Upload transport until the files contract is bound; only the returned record id is stored.</summary>
public interface IFileUploader
{
    Task<string> UploadAsync(UploadFile file, IProgress<decimal> progress, CancellationToken cancellationToken);
}
