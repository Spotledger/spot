using System;

namespace Spotledger.Client.Expressions;

/// <summary>A calendar date, years 0001 to 9999, stored as days since 1970-01-01.</summary>
public readonly struct ExpressionDate : IEquatable<ExpressionDate>, IComparable<ExpressionDate>
{
    /// <summary>The first day: 0001-01-01, as days since 1970-01-01.</summary>
    public const int MinDays = -719_162;

    /// <summary>The last day: 9999-12-31, as days since 1970-01-01.</summary>
    public const int MaxDays = 2_932_896;

    /// <summary>Creates the date <paramref name="days" /> after 1970-01-01, or nothing outside year 0001 to 9999.</summary>
    public static ExpressionDate? FromDays(int days) =>
        days >= MinDays && days <= MaxDays ? new ExpressionDate(days) : null;

    /// <summary>Creates a date from year, month (1 to 12) and day, or nothing if there is no such day.</summary>
    public static ExpressionDate? FromYearMonthDay(int year, int month, int day)
    {
        if (year < 1 || year > 9999 || month < 1 || month > 12 || day == 0)
        {
            return null;
        }
        if (day > DaysInMonth(year, month))
        {
            return null;
        }
        return FromDays(DaysFromCivil(year, month, day));
    }

    /// <summary>Parses <c>YYYY-MM-DD</c> exactly, or nothing.</summary>
    public static ExpressionDate? Parse(string text)
    {
        if (text.Length != 10 || text[4] != '-' || text[7] != '-')
        {
            return null;
        }
        int year = Digits(text, 0, 4);
        int month = Digits(text, 5, 2);
        int day = Digits(text, 8, 2);
        if (year < 0 || month < 0 || day < 0)
        {
            return null;
        }
        return FromYearMonthDay(year, month, day);
    }

    internal ExpressionDate(int days) => Days = days;

    /// <summary>Days since 1970-01-01.</summary>
    public int Days { get; }

    /// <summary>Year, month and day.</summary>
    public (int Year, int Month, int Day) YearMonthDay => CivilFromDays(Days);

    /// <summary>Whether this date equals <paramref name="other" />.</summary>
    public bool Equals(ExpressionDate other) => Days == other.Days;

    /// <summary>Whether <paramref name="obj" /> is an equal date.</summary>
    public override bool Equals(object? obj) => obj is ExpressionDate other && Equals(other);

    /// <summary>A hash based on the day number.</summary>
    public override int GetHashCode() => Days;

    /// <summary>The date's order.</summary>
    public int CompareTo(ExpressionDate other) => Days.CompareTo(other.Days);

    /// <summary>Whether two dates are the same day.</summary>
    public static bool operator ==(ExpressionDate a, ExpressionDate b) => a.Days == b.Days;

    /// <summary>Whether two dates differ.</summary>
    public static bool operator !=(ExpressionDate a, ExpressionDate b) => a.Days != b.Days;

    /// <summary>Whether <paramref name="a" /> is before <paramref name="b" />.</summary>
    public static bool operator <(ExpressionDate a, ExpressionDate b) => a.Days < b.Days;

    /// <summary>Whether <paramref name="a" /> is at or before <paramref name="b" />.</summary>
    public static bool operator <=(ExpressionDate a, ExpressionDate b) => a.Days <= b.Days;

    /// <summary>Whether <paramref name="a" /> is after <paramref name="b" />.</summary>
    public static bool operator >(ExpressionDate a, ExpressionDate b) => a.Days > b.Days;

    /// <summary>Whether <paramref name="a" /> is at or after <paramref name="b" />.</summary>
    public static bool operator >=(ExpressionDate a, ExpressionDate b) => a.Days >= b.Days;

    /// <summary>The canonical text: <c>YYYY-MM-DD</c>.</summary>
    public override string ToString()
    {
        (int y, int m, int d) = YearMonthDay;
        return Digits4(y) + "-" + TwoDigits(m) + "-" + TwoDigits(d);
    }

    internal static int DaysInMonth(int year, int month)
    {
        switch (month)
        {
            case 1:
            case 3:
            case 5:
            case 7:
            case 8:
            case 10:
            case 12:
                return 31;
            case 4:
            case 6:
            case 9:
            case 11:
                return 30;
            default:
                return IsLeap(year) ? 29 : 28;
        }
    }

    internal static bool IsLeap(int year) => (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;

    /// <summary>Days since 1970-01-01 of a proleptic Gregorian date.</summary>
    internal static int DaysFromCivil(int year, int month, int day)
    {
        int y = month <= 2 ? year - 1 : year;
        int era = FloorDiv(y, 400);
        int yoe = y - era * 400;
        int doy = (153 * (month > 2 ? month - 3 : month + 9) + 2) / 5 + day - 1;
        int doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        return era * 146_097 + doe - 719_468;
    }

    /// <summary>Year, month and day of days since 1970-01-01.</summary>
    internal static (int Year, int Month, int Day) CivilFromDays(int days)
    {
        int z = days + 719_468;
        int era = FloorDiv(z, 146_097);
        int doe = z - era * 146_097;
        int yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        int doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        int mp = (5 * doy + 2) / 153;
        int d = doy - (153 * mp + 2) / 5 + 1;
        int m = mp < 10 ? mp + 3 : mp - 9;
        int y = yoe + era * 400 + (m <= 2 ? 1 : 0);
        return (y, m, d);
    }

    internal static string Digits4(int value) =>
        ((value / 1000) % 10).ToString(provider: null) + ((value / 100) % 10).ToString(provider: null)
        + ((value / 10) % 10).ToString(provider: null) + (value % 10).ToString(provider: null);

    internal static string TwoDigits(int value) =>
        ((value / 10) % 10).ToString(provider: null) + (value % 10).ToString(provider: null);

    private static int Digits(string text, int start, int count)
    {
        int value = 0;
        for (int i = start; i < start + count; i++)
        {
            char c = text[i];
            if (c < '0' || c > '9')
            {
                return -1;
            }
            value = value * 10 + (c - '0');
        }
        return value;
    }

    private static int FloorDiv(int a, int b)
    {
        int q = a / b;
        if ((a % b != 0) && ((a < 0) != (b < 0)))
        {
            q--;
        }
        return q;
    }
}

/// <summary>An instant in UTC with microsecond precision, stored as microseconds since 1970-01-01T00:00:00Z.</summary>
public readonly struct ExpressionDateTime : IEquatable<ExpressionDateTime>, IComparable<ExpressionDateTime>
{
    /// <summary>Microseconds in a day (86400000000).</summary>
    public const long MicrosPerDay = 86_400_000_000L;

    /// <summary>Creates the instant <paramref name="micros" /> after 1970-01-01T00:00:00Z, or nothing outside year 0001 to 9999.</summary>
    public static ExpressionDateTime? FromMicros(long micros)
    {
        long lo = (long)ExpressionDate.MinDays * MicrosPerDay;
        long hi = ((long)ExpressionDate.MaxDays + 1) * MicrosPerDay - 1;
        return micros >= lo && micros <= hi ? new ExpressionDateTime(micros) : null;
    }

    /// <summary>Parses <c>YYYY-MM-DDTHH:MM:SS[.f{1,6}]Z</c> exactly, or nothing.</summary>
    public static ExpressionDateTime? Parse(string text)
    {
        if (text.Length < 20 || text[10] != 'T' || text[text.Length - 1] != 'Z')
        {
            return null;
        }
        ExpressionDate? date = ExpressionDate.Parse(text.Substring(0, 10));
        if (date is null || text[13] != ':' || text[16] != ':')
        {
            return null;
        }
        int hour = DigitPair(text, 11);
        int minute = DigitPair(text, 14);
        int second = DigitPair(text, 17);
        if (hour < 0 || minute < 0 || second < 0 || hour > 23 || minute > 59 || second > 59)
        {
            return null;
        }
        long micros = 0;
        int restStart = 19;
        int restEnd = text.Length - 1;
        if (restStart < restEnd)
        {
            if (text[restStart] != '.')
            {
                return null;
            }
            int fracLen = restEnd - (restStart + 1);
            if (fracLen < 1 || fracLen > 6)
            {
                return null;
            }
            micros = 0;
            for (int i = restStart + 1; i < restEnd; i++)
            {
                char c = text[i];
                if (c < '0' || c > '9')
                {
                    return null;
                }
                micros = micros * 10 + (c - '0');
            }
            for (int i = fracLen; i < 6; i++)
            {
                micros *= 10;
            }
        }
        else if (restStart != restEnd)
        {
            return null;
        }
        long seconds = hour * 3600L + minute * 60L + second;
        return FromMicros((long)date.Value.Days * MicrosPerDay + seconds * 1_000_000L + micros);
    }

    internal ExpressionDateTime(long micros) => Micros = micros;

    /// <summary>Microseconds since 1970-01-01T00:00:00Z.</summary>
    public long Micros { get; }

    /// <summary>The UTC date of the instant.</summary>
    public ExpressionDate Date => new ExpressionDate((int)FloorDiv(Micros, MicrosPerDay));

    /// <summary>The midnight UTC of a date.</summary>
    public static ExpressionDateTime FromDate(ExpressionDate date) =>
        new ExpressionDateTime((long)date.Days * MicrosPerDay);

    /// <summary>Whether this instant equals <paramref name="other" />.</summary>
    public bool Equals(ExpressionDateTime other) => Micros == other.Micros;

    /// <summary>Whether <paramref name="obj" /> is an equal instant.</summary>
    public override bool Equals(object? obj) => obj is ExpressionDateTime other && Equals(other);

    /// <summary>A hash based on the microsecond number.</summary>
    public override int GetHashCode() => Micros.GetHashCode();

    /// <summary>The instant's order.</summary>
    public int CompareTo(ExpressionDateTime other) => Micros.CompareTo(other.Micros);

    /// <summary>Whether two instants are the same.</summary>
    public static bool operator ==(ExpressionDateTime a, ExpressionDateTime b) => a.Micros == b.Micros;

    /// <summary>Whether two instants differ.</summary>
    public static bool operator !=(ExpressionDateTime a, ExpressionDateTime b) => a.Micros != b.Micros;

    /// <summary>Whether <paramref name="a" /> is before <paramref name="b" />.</summary>
    public static bool operator <(ExpressionDateTime a, ExpressionDateTime b) => a.Micros < b.Micros;

    /// <summary>Whether <paramref name="a" /> is at or before <paramref name="b" />.</summary>
    public static bool operator <=(ExpressionDateTime a, ExpressionDateTime b) => a.Micros <= b.Micros;

    /// <summary>Whether <paramref name="a" /> is after <paramref name="b" />.</summary>
    public static bool operator >(ExpressionDateTime a, ExpressionDateTime b) => a.Micros > b.Micros;

    /// <summary>Whether <paramref name="a" /> is at or after <paramref name="b" />.</summary>
    public static bool operator >=(ExpressionDateTime a, ExpressionDateTime b) => a.Micros >= b.Micros;

    /// <summary>The canonical text: <c>YYYY-MM-DDTHH:MM:SSZ</c>, the fraction (always six digits) only when non-zero.</summary>
    public override string ToString()
    {
        long dayMicros = FloorMod(Micros, MicrosPerDay);
        long secs = dayMicros / 1_000_000L;
        long micros = dayMicros % 1_000_000L;
        string baseText = Date.ToString()
            + "T" + ExpressionDate.TwoDigits((int)(secs / 3600))
            + ":" + ExpressionDate.TwoDigits((int)((secs / 60) % 60))
            + ":" + ExpressionDate.TwoDigits((int)(secs % 60));
        if (micros == 0)
        {
            return baseText + "Z";
        }
        return baseText + "." + SixDigits(micros) + "Z";
    }

    private static string SixDigits(long micros) =>
        ExpressionDate.TwoDigits((int)(micros / 10000))
        + ExpressionDate.TwoDigits((int)((micros / 100) % 100))
        + ExpressionDate.TwoDigits((int)(micros % 100));

    private static int DigitPair(string text, int start)
    {
        char a = text[start];
        char b = text[start + 1];
        if (a < '0' || a > '9' || b < '0' || b > '9')
        {
            return -1;
        }
        return (a - '0') * 10 + (b - '0');
    }

    private static long FloorDiv(long a, long b)
    {
        long q = a / b;
        if ((a % b != 0) && ((a < 0) != (b < 0)))
        {
            q--;
        }
        return q;
    }

    private static long FloorMod(long a, long b)
    {
        long r = a % b;
        if (r != 0 && ((r < 0) != (b < 0)))
        {
            r += b;
        }
        return r;
    }
}