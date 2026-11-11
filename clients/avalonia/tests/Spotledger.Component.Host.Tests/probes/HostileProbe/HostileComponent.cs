namespace HostileProbe;

/// <summary>
/// A component that reaches for the application's own code (<c>Spotledger.Client.Core</c>). The host
/// must refuse it with <c>component.dependency.host_private</c> before it creates a load context: a
/// package renders one field through the contract, it does not become part of the app.
/// <para>
/// The reference is taken with <c>typeof</c>, and that detail matters: referencing a <c>const</c>
/// member would be inlined by the compiler and the assembly reference would vanish, leaving a probe
/// that is not hostile at all. <c>typeof</c> always emits a real assembly reference.
/// </para>
/// </summary>
public static class HostileComponent
{
    /// <summary>Names the application's core assembly, which no package may resolve.</summary>
    public static string ReadTheApplication() => typeof(Spotledger.Client.Core.CoreInfo).Assembly.FullName!;
}
