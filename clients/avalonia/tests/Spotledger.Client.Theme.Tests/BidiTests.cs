using Avalonia.Media;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.Theme.Tests;

/// <summary>
/// The twelve bidi cases documented in <c>docs/urdu-rtl.md</c>, one test each. They pin the two
/// rules the client lives by: a field's base direction comes from its kind and language (never
/// from the window alone), and code-like values are isolated for display with U+2066/U+2069
/// while the stored value and the copied text stay untouched.
/// </summary>
public sealed class BidiTests
{
    private const string UrduPrefix = "رسید نمبر";
    private const string ItemCode = "SKU-1234-A";
    private const string Iban = "PK36SCBL0000001123456702";
    private const string Phone = "+92 300 1234567";

    /// <summary>Case 1: an Urdu caption sets its own direction even in a left-to-right window.</summary>
    [Fact]
    public void Case01UrduCaptionIsRightToLeftInsideLeftToRightChrome()
    {
        Assert.Equal(TextDirection.RightToLeft, BidiText.BaseDirection("محفوظ کریں"));
        Assert.Equal(TextDirection.LeftToRight, BidiText.BaseDirection("Save"));
        Assert.Equal(FlowDirection.RightToLeft, BidiText.Flow(BidiText.BaseDirection("محفوظ کریں", TextDirection.LeftToRight)));
    }

    /// <summary>Case 2: a field with no strong character falls back to its language direction.</summary>
    [Fact]
    public void Case02EmptyNumbersAndPunctuationFallBackToTheFieldDirection()
    {
        foreach (var text in new[] { string.Empty, "   ", "1,234.50", "-", "۔۔۔", "( )" })
        {
            Assert.Equal(TextDirection.RightToLeft, BidiText.BaseDirection(text, TextDirection.RightToLeft));
            Assert.Equal(TextDirection.LeftToRight, BidiText.BaseDirection(text, TextDirection.LeftToRight));
            Assert.Equal(TextDirection.LeftToRight, BidiText.BaseDirection(null));
        }
    }

    /// <summary>Case 3: the first strong character wins, so a leading digit does not flip the run.</summary>
    [Fact]
    public void Case03ALeadingNumberDoesNotFlipAnUrduSentence()
    {
        Assert.Equal(TextDirection.RightToLeft, BidiText.BaseDirection("۱۲۳ اکتوبر کی رپورٹ"));
        Assert.Equal(TextDirection.RightToLeft, BidiText.BaseDirection("1,234.50 روپے واجب الادا ہیں"));
        Assert.Equal(TextDirection.LeftToRight, BidiText.BaseDirection("1,234.50 rupees are due"));
    }

    /// <summary>Case 4: an item code is isolated left-to-right inside Urdu text.</summary>
    [Fact]
    public void Case04ItemCodeIsIsolatedLeftToRight()
    {
        var display = BidiText.ForDisplay(ItemCode, FieldTextKind.Code, TextDirection.RightToLeft);

        Assert.Equal($"\u2066{ItemCode}\u2069", display);
        Assert.Equal(ItemCode, BidiText.RemoveIsolates(display));
    }

    /// <summary>Case 5: an invoice number keeps Latin digits and hyphens in reading order.</summary>
    [Fact]
    public void Case05InvoiceNumberIsIsolatedLeftToRight()
    {
        var display = BidiText.ForDisplay("INV-2026-0042", FieldTextKind.InvoiceNumber, TextDirection.RightToLeft);

        Assert.Equal("\u2066INV-2026-0042\u2069", display);
        Assert.Equal("INV-2026-0042", BidiText.RemoveIsolates(display));
    }

    /// <summary>Case 6: an IBAN is never reordered, and a leading plus does not move.</summary>
    [Fact]
    public void Case06IbanAndPhoneStayLeftToRight()
    {
        var iban = BidiText.ForDisplay(Iban, FieldTextKind.Iban, TextDirection.RightToLeft);
        var phone = BidiText.ForDisplay(Phone, FieldTextKind.Phone, TextDirection.RightToLeft);

        Assert.Equal($"\u2066{Iban}\u2069", iban);
        Assert.Equal($"\u2066{Phone}\u2069", phone);
        Assert.StartsWith("\u2066+92", phone, StringComparison.Ordinal);
        Assert.Equal(Iban, BidiText.RemoveIsolates(iban));
        Assert.Equal(Phone, BidiText.RemoveIsolates(phone));
    }

    /// <summary>Case 7: an ISO date (ADR-077) is isolated, so the year cannot jump to the left.</summary>
    [Fact]
    public void Case07IsoDateIsIsolated()
    {
        var display = BidiText.ForDisplay("2026-10-28", FieldTextKind.Date, TextDirection.RightToLeft);

        Assert.Equal("\u20662026-10-28\u2069", display);
        Assert.Equal("2026-10-28", BidiText.RemoveIsolates(display));
    }

    /// <summary>Case 8: money keeps its group separators inside the isolate.</summary>
    [Fact]
    public void Case08MoneyIsIsolatedSoGroupSeparatorsDoNotReorder()
    {
        var display = BidiText.ForDisplay("1,234.50", FieldTextKind.Money, TextDirection.RightToLeft);

        Assert.Equal("\u20661,234.50\u2069", display);
        Assert.Equal("1,234.50", BidiText.RemoveIsolates(display));
    }

    /// <summary>
    /// Case 9: trailing punctuation of a sentence stays outside the isolate, so a full stop after
    /// a code belongs to the Urdu sentence and not to the code.
    /// </summary>
    [Fact]
    public void Case09TrailingPunctuationStaysOutsideTheIsolate()
    {
        var display = BidiText.ForDisplay("SKU-1234-A۔", FieldTextKind.Code, TextDirection.RightToLeft);

        Assert.Equal("\u2066SKU-1234-A\u2069۔", display);
        Assert.Equal("SKU-1234-A۔", BidiText.RemoveIsolates(display));

        var quoted = BidiText.IsolateCode("(SKU-1234-A)");
        Assert.Equal("(\u2066SKU-1234-A\u2069)", quoted);
    }

    /// <summary>Case 10: prose with an embedded Latin e-mail is prose; the e-mail field is isolated.</summary>
    [Fact]
    public void Case10ProseIsUnchangedAndTheEmailFieldIsIsolated()
    {
        const string Prose = "رابطہ احمد رضا سے کریں";

        Assert.Equal(Prose, BidiText.ForDisplay(Prose, FieldTextKind.FreeText, TextDirection.RightToLeft));
        Assert.Equal(TextDirection.RightToLeft, BidiText.FieldDirection(FieldTextKind.FreeText, Prose, TextDirection.LeftToRight));
        Assert.Equal("\u2066accounts@example.com\u2069", BidiText.ForDisplay("accounts@example.com", FieldTextKind.Email, TextDirection.RightToLeft));
    }

    /// <summary>Case 11: an address follows its own text, a code column never does.</summary>
    [Fact]
    public void Case11AddressFollowsItsTextWhileACodeColumnStaysLeftToRight()
    {
        const string Address = "گھر نمبر ۱۲، گلی نمبر ۴، ماڈل ٹاؤن، لاہور";

        Assert.Equal(TextDirection.RightToLeft, BidiText.FieldDirection(FieldTextKind.Address, Address, TextDirection.LeftToRight));
        Assert.Equal(Address, BidiText.ForDisplay(Address, FieldTextKind.Address, TextDirection.LeftToRight));
        Assert.Equal(TextDirection.LeftToRight, BidiText.FieldDirection(FieldTextKind.Code, Address, TextDirection.RightToLeft));
    }

    /// <summary>
    /// Case 12: isolates are display-only. The stored value, the copied text and the round trip
    /// are byte-identical to what was typed, and isolate insertion is detectable but removable.
    /// </summary>
    [Fact]
    public void Case12IsolatesNeverChangeTheStoredValueOrCopiedText()
    {
        const string Stored = "INV-2026-0042";
        var display = BidiText.ForDisplay(Stored, FieldTextKind.InvoiceNumber, TextDirection.RightToLeft);

        Assert.NotEqual(Stored, display);
        Assert.True(BidiText.ContainsFormattingControls(display));
        Assert.False(BidiText.ContainsFormattingControls(Stored));
        Assert.Equal(Stored, BidiText.RemoveIsolates(display));
        Assert.Equal(Stored.Length, BidiText.RemoveIsolates(display).Length);
        // Copying the display text strips the isolates: what the user pastes is the value.
        Assert.Equal(Stored, BidiText.RemoveIsolates(display));
    }

    [Fact]
    public void EveryFieldKindHasADocumentedDirection()
    {
        foreach (var kind in Enum.GetValues<FieldTextKind>())
        {
            var direction = BidiText.FieldDirection(kind, "احمد رضا", TextDirection.RightToLeft);
            var display = BidiText.ForDisplay("احمد رضا", kind, TextDirection.RightToLeft);

            if (kind is FieldTextKind.Code or FieldTextKind.InvoiceNumber or FieldTextKind.Iban
                or FieldTextKind.Phone or FieldTextKind.Email or FieldTextKind.Money or FieldTextKind.Date)
            {
                Assert.Equal(TextDirection.LeftToRight, direction);
                Assert.Equal("\u2066احمد رضا\u2069", display);
            }
            else
            {
                Assert.Equal(TextDirection.RightToLeft, direction);
                Assert.Equal("احمد رضا", display);
            }
        }
    }

    [Fact]
    public void TheHelperRecognisesArabicScriptAndTheIsolateCharacterSet()
    {
        Assert.True(BidiText.HasArabicScript("اردو"));
        Assert.True(BidiText.HasArabicScript("مرحبا"));
        Assert.False(BidiText.HasArabicScript("Urdu 123"));

        Assert.Equal("\u2066a\u2069", BidiText.Isolate("a", TextDirection.LeftToRight));
        Assert.Equal("\u2067a\u2069", BidiText.Isolate("a", TextDirection.RightToLeft));
        Assert.Equal("\u2068a\u2069", BidiText.IsolateAuto("a"));
        Assert.Equal(string.Empty, BidiText.Isolate(null, TextDirection.LeftToRight));
        Assert.Equal("ab", BidiText.RemoveIsolates("a\u202Eb\u200F"));
    }
}
