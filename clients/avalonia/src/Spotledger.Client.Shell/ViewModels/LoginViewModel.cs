using Spotledger.Client.Api;

namespace Spotledger.Client.Shell;

/// <summary>
/// Step two: user and password against <c>POST /api/v1/auth/login</c>. The contract's
/// error text shows up as the form message and, when the server sends them, as per-field
/// messages. A <c>auth.totp_required</c> answer reveals the code field for a retry
/// (the contract's LoginRequest carries <c>totp</c>). The password is dropped as soon as
/// the call returns; the session token stays in memory unless "Remember me" is ticked —
/// then the token (never the password) goes to the OS secret store under
/// <c>&lt;server&gt;/&lt;user&gt;</c> (WP-UI-02.T3). "Remember me" is off by default.
/// </summary>
public sealed class LoginViewModel : ObservableObject
{
    /// <summary>The contract code that means a one-time code is required (WP-CON-03 registry).</summary>
    public const string TotpRequiredCode = "auth.totp_required";

    private string _user = string.Empty;
    private string _password = string.Empty;
    private string _totpCode = string.Empty;
    private string? _formMessage;
    private string? _userFieldMessage;
    private string? _passwordFieldMessage;
    private string? _totpFieldMessage;
    private bool _needsTotp;
    private bool _rememberMe;

    private readonly ApiConnection _connection;

    public LoginViewModel(ApiConnection connection, ISecretStore? secretStore = null)
    {
        _connection = connection ?? throw new ArgumentNullException(nameof(connection));
        Secrets = secretStore ?? _connection.Secrets;
        SignInCommand = new AsyncRelayCommand(SignInAsync, _ => CanSignIn);
        BackCommand = new RelayCommand(_ => GoBack());
    }

    /// <summary>The secret store the "Remember me" tick writes to (injected; falls back to the connection's).</summary>
    public ISecretStore Secrets { get; }

    /// <summary>Raised after a successful sign-in; the shell owns the stage change.</summary>
    public event EventHandler? SignedIn;

    /// <summary>Raised when the user backs out to the server picker.</summary>
    public event EventHandler? BackRequested;

    /// <summary>The window title's site name, set by the app from the picked URL.</summary>
    public string ServerDisplay { get; set; } = string.Empty;

    /// <summary>The user a rejected resume prefilled; cleared after a successful sign-in.</summary>
    public string? PrefilledUser { get; private set; }

    public string User
    {
        get => _user;
        set
        {
            if (SetField(ref _user, value))
            {
                UserFieldMessage = null;
                SignInCommand.RaiseCanExecuteChanged();
            }
        }
    }

    public string Password
    {
        get => _password;
        set
        {
            if (SetField(ref _password, value))
            {
                PasswordFieldMessage = null;
                SignInCommand.RaiseCanExecuteChanged();
            }
        }
    }

    /// <summary>"Remember me": keep the session token in the OS keychain for the next start. Off by default.</summary>
    public bool RememberMe
    {
        get => _rememberMe;
        set => SetField(ref _rememberMe, value);
    }

    /// <summary>The once-per-run warning when no OS store exists ("You will be asked to sign in again next time.").</summary>
    public static string RememberMeNotice => SecretStores.FallbackNotice;

    /// <summary>True after the server answered <c>auth.totp_required</c>: the code field is visible.</summary>
    public bool NeedsTotp
    {
        get => _needsTotp;
        private set
        {
            SetField(ref _needsTotp, value);
            OnPropertyChanged(nameof(ShowTotpField));
        }
    }

    /// <summary>Alias for the view's bindable visibility.</summary>
    public bool ShowTotpField => NeedsTotp;

    public string TotpCode
    {
        get => _totpCode;
        set
        {
            if (SetField(ref _totpCode, value))
            {
                TotpFieldMessage = null;
            }
        }
    }

    /// <summary>The contract's message for the failed sign-in, shown on the form.</summary>
    public string? FormMessage
    {
        get => _formMessage;
        private set
        {
            SetField(ref _formMessage, value);
            OnPropertyChanged(nameof(HasFormMessage));
        }
    }

    public bool HasFormMessage => !string.IsNullOrEmpty(FormMessage);

    public string? UserFieldMessage
    {
        get => _userFieldMessage;
        private set
        {
            SetField(ref _userFieldMessage, value);
            OnPropertyChanged(nameof(HasUserFieldMessage));
        }
    }

    public bool HasUserFieldMessage => !string.IsNullOrEmpty(UserFieldMessage);

    public string? PasswordFieldMessage
    {
        get => _passwordFieldMessage;
        private set
        {
            SetField(ref _passwordFieldMessage, value);
            OnPropertyChanged(nameof(HasPasswordFieldMessage));
        }
    }

    public bool HasPasswordFieldMessage => !string.IsNullOrEmpty(PasswordFieldMessage);

    public string? TotpFieldMessage
    {
        get => _totpFieldMessage;
        private set
        {
            SetField(ref _totpFieldMessage, value);
            OnPropertyChanged(nameof(HasTotpFieldMessage));
        }
    }

    public bool HasTotpFieldMessage => !string.IsNullOrEmpty(TotpFieldMessage);

    public AsyncRelayCommand SignInCommand { get; }

    public RelayCommand BackCommand { get; }

    private bool CanSignIn =>
        !string.IsNullOrWhiteSpace(User) && !string.IsNullOrEmpty(Password);

    private async Task SignInAsync(object? parameter)
    {
        FormMessage = null;
        UserFieldMessage = null;
        PasswordFieldMessage = null;
        TotpFieldMessage = null;
        string? totp = NeedsTotp && !string.IsNullOrWhiteSpace(TotpCode) ? TotpCode.Trim() : null;
        var remember = RememberMe;
        try
        {
            // LoginAsync keeps the session token in memory; RememberTokenAsync copies it
            // to the OS secret store only when the user ticked "Remember me" — the value
            // is the token, never the password (which is dropped below in every path).
            await _connection.LoginAsync(User, Password, totp);
            if (remember)
            {
                await _connection.RememberTokenAsync(User);
            }

            Password = string.Empty;
            TotpCode = string.Empty;
            NeedsTotp = false;
            PrefilledUser = null;
            SignedIn?.Invoke(this, EventArgs.Empty);
        }
        catch (ApiRequestException failure)
        {
            Password = string.Empty;
            ShowContractError(failure.Error);
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            Password = string.Empty;
            FormMessage = "The sign-in request could not be completed. Try again.";
        }
        finally
        {
            // Cancellation during shutdown must not leave credentials in the form either.
            Password = string.Empty;
        }
    }

    private void ShowContractError(ApiError error)
    {
        if (error.Code == TotpRequiredCode)
        {
            NeedsTotp = true;
            TotpFieldMessage = null;
            FormMessage = error.Message;
            return;
        }

        FormMessage = error.Message;
        if (error.FieldErrors.Count > 0)
        {
            foreach (var field in error.FieldErrors)
            {
                switch (field.Field)
                {
                    case "user":
                        UserFieldMessage = field.Message;
                        break;
                    case "password":
                        PasswordFieldMessage = field.Message;
                        break;
                    case "totp":
                        TotpFieldMessage = field.Message;
                        NeedsTotp = true;
                        break;
                }
            }
        }
    }

    private void GoBack() => BackRequested?.Invoke(this, EventArgs.Empty);

    /// <summary>Fills the form from a rejected resume (the app calls it before showing the step).</summary>
    public void PrefillFromResume(string serverDisplay, string user)
    {
        ServerDisplay = serverDisplay;
        User = user;
        PrefilledUser = user;
    }
}
