#nullable disable
#pragma warning disable CA1510, CA1850 // written against the plain BCL: this file is also compiled by the MSBuild task factory
// The integrity manifest of the browser build (assets.json). This one file is used in two places:
//  * compiled into the head's tests (SPOT_MANIFEST_NO_TASK defined), which test AssetManifest;
//  * compiled by MSBuild's inline task factory during `dotnet publish` (Spotledger.Client.Browser.csproj),
//    where it also provides the WriteAssetManifest task.
// It therefore uses the base class library only and no file outside this one.
using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Security.Cryptography;
using System.Text;

namespace Spotledger.Client.Browser.Build
{
    /// <summary>One published file in the manifest.</summary>
    public sealed class AssetEntry
    {
        /// <summary>Creates the entry.</summary>
        public AssetEntry(string path, long bytes, string sha256)
        {
            Path = path;
            Bytes = bytes;
            Sha256 = sha256;
        }

        /// <summary>The path relative to the published folder, with '/' separators.</summary>
        public string Path { get; }

        /// <summary>The size in bytes.</summary>
        public long Bytes { get; }

        /// <summary>The SHA-256 of the content, lower-case hex.</summary>
        public string Sha256 { get; }
    }

    /// <summary>
    /// Builds the manifest of a published folder: every file (the pre-compressed .br and .gz siblings
    /// too) with its size and SHA-256, sorted by path, plus one <c>build</c> id that is the hash of all of
    /// them. No timestamp or machine name goes in, so the same files always give the same bytes.
    /// </summary>
    public static class AssetManifest
    {
        /// <summary>The manifest file name; it is never listed in itself.</summary>
        public const string FileName = "assets.json";

        /// <summary>The manifest format version.</summary>
        public const int Version = 1;

        /// <summary>Lists the files under <paramref name="root"/> (except the manifest itself), sorted by path.</summary>
        public static IReadOnlyList<AssetEntry> Scan(string root)
        {
            if (root == null)
            {
                throw new ArgumentNullException(nameof(root));
            }

            string full = System.IO.Path.GetFullPath(root);
            var entries = new List<AssetEntry>();
            foreach (string file in Directory.GetFiles(full, "*", SearchOption.AllDirectories))
            {
                string relative = file.Substring(full.TrimEnd(System.IO.Path.DirectorySeparatorChar).Length + 1)
                    .Replace(System.IO.Path.DirectorySeparatorChar, '/');
                if (string.Equals(relative, FileName, StringComparison.Ordinal))
                {
                    continue;
                }

                using (FileStream stream = File.OpenRead(file))
                using (SHA256 sha = SHA256.Create())
                {
                    string hash = ToHex(sha.ComputeHash(stream));
                    entries.Add(new AssetEntry(relative, new FileInfo(file).Length, hash));
                }
            }

            entries.Sort((a, b) => string.CompareOrdinal(a.Path, b.Path));
            return entries;
        }

        /// <summary>The build id: the SHA-256 of every entry's path and hash in path order.</summary>
        public static string BuildId(IReadOnlyList<AssetEntry> entries)
        {
            if (entries == null)
            {
                throw new ArgumentNullException(nameof(entries));
            }

            var text = new StringBuilder();
            foreach (AssetEntry entry in entries)
            {
                text.Append(entry.Path).Append('\0').Append(entry.Sha256).Append('\n');
            }

            using (SHA256 sha = SHA256.Create())
            {
                return ToHex(sha.ComputeHash(Encoding.UTF8.GetBytes(text.ToString())));
            }
        }

        /// <summary>The manifest JSON for <paramref name="entries"/> (LF line ends, no trailing spaces).</summary>
        public static string ToJson(IReadOnlyList<AssetEntry> entries)
        {
            if (entries == null)
            {
                throw new ArgumentNullException(nameof(entries));
            }

            var json = new StringBuilder();
            json.Append("{\n");
            json.Append("  \"version\": ").Append(Version.ToString(CultureInfo.InvariantCulture)).Append(",\n");
            json.Append("  \"build\": \"").Append(BuildId(entries)).Append("\",\n");
            json.Append("  \"files\": [");
            for (int i = 0; i < entries.Count; i++)
            {
                AssetEntry entry = entries[i];
                json.Append(i == 0 ? "\n" : ",\n");
                json.Append("    { \"path\": \"").Append(Escape(entry.Path)).Append("\", \"bytes\": ")
                    .Append(entry.Bytes.ToString(CultureInfo.InvariantCulture))
                    .Append(", \"sha256\": \"").Append(entry.Sha256).Append("\" }");
            }

            json.Append(entries.Count == 0 ? "]\n" : "\n  ]\n");
            json.Append("}\n");
            return json.ToString();
        }

        /// <summary>Scans <paramref name="root"/> and writes <c>assets.json</c> into it. Returns the manifest path.</summary>
        public static string Write(string root)
        {
            string path = System.IO.Path.Combine(root, FileName);
            File.WriteAllText(path, ToJson(Scan(root)), new UTF8Encoding(false));
            return path;
        }

        private static string ToHex(byte[] bytes)
        {
            var hex = new StringBuilder(bytes.Length * 2);
            foreach (byte b in bytes)
            {
                hex.Append(b.ToString("x2", CultureInfo.InvariantCulture));
            }

            return hex.ToString();
        }

        private static string Escape(string text)
        {
            var escaped = new StringBuilder(text.Length);
            foreach (char c in text)
            {
                if (c == '"' || c == '\\')
                {
                    escaped.Append('\\').Append(c);
                }
                else if (c < ' ')
                {
                    escaped.Append("\\u").Append(((int)c).ToString("x4", CultureInfo.InvariantCulture));
                }
                else
                {
                    escaped.Append(c);
                }
            }

            return escaped.ToString();
        }
    }

#if !SPOT_MANIFEST_NO_TASK
    /// <summary>The MSBuild task that runs <see cref="AssetManifest.Write"/> over the published folder.</summary>
    public sealed class WriteAssetManifest : Microsoft.Build.Utilities.Task
    {
        /// <summary>The published folder that is served (the one holding index.html).</summary>
        [Microsoft.Build.Framework.Required]
        public string Root { get; set; }

        /// <summary>The path of the written manifest.</summary>
        [Microsoft.Build.Framework.Output]
        public string ManifestPath { get; set; }

        /// <inheritdoc />
        public override bool Execute()
        {
            if (!Directory.Exists(Root))
            {
                Log.LogError("The published folder '{0}' does not exist; the manifest cannot be written.", Root);
                return false;
            }

            ManifestPath = AssetManifest.Write(Root);
            Log.LogMessage(Microsoft.Build.Framework.MessageImportance.High, "Wrote {0}", ManifestPath);
            return true;
        }
    }
#endif
}
