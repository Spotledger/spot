using System.Text;
using Spotledger.Component.Contract;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// The canonical JSON writer: keys sorted, no whitespace, integers only, and serde_json's exact
/// escaping. These are the rules the Rust crate and the Python shell tool implement, so a byte
/// difference here is a signature that fails to verify on the other side.
/// </summary>
public sealed class CanonicalJsonTests
{
    [Fact]
    public void ObjectKeysAreSortedAndWrittenWithoutWhitespace()
    {
        var value = CanonicalObject.Of(
            ("zeta", CanonicalJson.Text("last")),
            ("alpha", CanonicalJson.Text("first")),
            ("Middle", CanonicalJson.Text("byte order")));

        Assert.Equal("{\"Middle\":\"byte order\",\"alpha\":\"first\",\"zeta\":\"last\"}", CanonicalJson.Write(value));
    }

    [Fact]
    public void NestedObjectsAndArraysKeepTheOrderTheyAreGiven()
    {
        var value = CanonicalObject.Of(
            ("files", CanonicalArray.Of(new[]
            {
                CanonicalObject.Of(("path", CanonicalJson.Text("b")), ("sha256", CanonicalJson.Text("2"))),
                CanonicalObject.Of(("path", CanonicalJson.Text("a")), ("sha256", CanonicalJson.Text("1"))),
            })),
            ("range", CanonicalObject.Of(("maximum_exclusive", CanonicalJson.Text("2.0")), ("minimum", CanonicalJson.Text("1.0")))));

        Assert.Equal(
            "{\"files\":[{\"path\":\"b\",\"sha256\":\"2\"},{\"path\":\"a\",\"sha256\":\"1\"}],"
            + "\"range\":{\"maximum_exclusive\":\"2.0\",\"minimum\":\"1.0\"}}",
            CanonicalJson.Write(value));
    }

    [Fact]
    public void StringsAreEscapedTheWaySerdeJsonEscapesThem()
    {
        var builder = new StringBuilder();
        CanonicalJson.WriteString(builder, "Quote \" backslash \\ newline \n tab \t end \u0001 \u001f\u007f \b\f\r é ü 星");

        Assert.Equal(
            "\"Quote \\\" backslash \\\\ newline \\n tab \\t end \\u0001 \\u001f\u007f \\b\\f\\r é ü 星\"",
            builder.ToString());
    }

    [Fact]
    public void ControlCharactersUseLowerCaseHexAndDelIsNotEscaped()
    {
        var builder = new StringBuilder();
        CanonicalJson.WriteString(builder, "\u001f\u007f\b\f\r");

        Assert.Equal("\"\\u001f\u007f\\b\\f\\r\"", builder.ToString());
    }

    [Fact]
    public void DuplicateKeysAreRefused()
    {
        var error = Assert.Throws<ArgumentException>(() => CanonicalObject.Of(
            ("twice", CanonicalJson.Text("a")),
            ("twice", CanonicalJson.Text("b"))));

        Assert.Contains("twice", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void NumbersAreIntegersOnlyAndBooleansAreLiterals()
    {
        var value = CanonicalObject.Of(
            ("count", CanonicalJson.Number(-12)),
            ("compromised", CanonicalJson.Flag(true)),
            ("sha", CanonicalJson.Number(0)));

        Assert.Equal("{\"compromised\":true,\"count\":-12,\"sha\":0}", CanonicalJson.Write(value));
    }

    [Fact]
    public void EncodingPrependsTheDomainBytes()
    {
        var value = CanonicalObject.Of(("a", CanonicalJson.Text("b")));
        var signed = CanonicalJson.Encode(CanonicalJson.ComponentManifestDomain, value);

        Assert.Equal("SPOT-COMPONENT-MANIFEST-V1\n{\"a\":\"b\"}", Encoding.UTF8.GetString(signed));
    }

    [Fact]
    public void TheDomainPrefixesAreTheRustCrates()
    {
        Assert.Equal("SPOT-COMPONENT-MANIFEST-V1\n", CanonicalJson.ComponentManifestDomain);
        Assert.Equal("SPOT-PUBLISHER-CERT-V1\n", CanonicalJson.CertificateDomain);
        Assert.Equal("SPOT-REVOCATION-LIST-V1\n", CanonicalJson.RevocationDomain);
    }

    [Fact]
    public void AKeyIdIsTheFirstSixteenBytesOfTheSha256InLowercaseHex()
    {
        var key = Vectors.Load().PublicKey("publisher");
        var keyId = Ed25519.KeyIdOf(key);

        Assert.Equal(32, keyId.Length);
        Assert.Equal(keyId.ToLowerInvariant(), keyId);
        Assert.True(Ed25519.IsKeyId(keyId));
        Assert.False(Ed25519.IsKeyId(keyId.ToUpperInvariant()));
    }

    [Fact]
    public void AComponentVersionRoundTripsThroughTheManifest()
    {
        Assert.True(ComponentVersion.TryParse("2.3.1", out var version));
        Assert.Equal("2.3.1", version.ToString());
        Assert.Equal("1.2.0", ComponentVersion.Parse("1.2").ToString());
    }
}
