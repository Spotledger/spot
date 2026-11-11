using Spotledger.Client.Api;

namespace Spotledger.Client.Shell;

/// <summary>
/// The "change password" dialog (WP-UI-02.T3): current and new password against
/// <c>POST /api/v1/auth/password</c> (contract v0.3.0 — the connection's hand-written
/// plumbing). The server's policy violations arrive as per-field errors and land under
/// their fields; the passwords are dropped the moment the call returns, and only the
/// request body ever carries them.
/// </summary>
public sealed class ChangePasswordViewModel : ObservableObject
{
    private string _currentPassword = string.Empty;
    private string _newPassword = string.Empty;
    private string? _formMessage;
    private string? _currentFieldMessage;
    private string? _newFieldMessage;
    private bool _isSaving;
    private readonly ApiConnection _connection;

    public ChangePasswordViewModel(ApiConnection connection)
    {
        _connection = connection ?? throw new ArgumentNullException(nameof(connection));
        SaveCommand = new AsyncRelayCommand(SaveAsync, _ => CanSave && !_isSaving);
        CancelCommand = new RelayCommand(_ => Close());
    }

    /// <summary>Raised after the password changed (or the dialog was dismissed); the view closes.</summary>
    public event EventHandler? CloseRequested;

    public string CurrentPassword
    {
        get => _currentPassword;
        set
        {
            if (SetField(ref _currentPassword, value))
            {
                CurrentFieldMessage = null;
                SaveCommand.RaiseCanExecuteChanged();
            }
        }
    }

    public string NewPassword
    {
        get => _newPassword;
        set
        {
            if (SetField(ref _newPassword, value))
            {
                NewFieldMessage = null;
                SaveCommand.RaiseCanExecuteChanged();
            }
        }
    }

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

    public string? CurrentFieldMessage
    {
        get => _currentFieldMessage;
        private set
        {
            SetField(ref _currentFieldMessage, value);
            OnPropertyChanged(nameof(HasCurrentFieldMessage));
        }
    }

    public bool HasCurrentFieldMessage => !string.IsNullOrEmpty(CurrentFieldMessage);

    public string? NewFieldMessage
    {
        get => _newFieldMessage;
        private set
        {
            SetField(ref _newFieldMessage, value);
            OnPropertyChanged(nameof(HasNewFieldMessage));
        }
    }

    public bool HasNewFieldMessage => !string.IsNullOrEmpty(NewFieldMessage);

    public bool IsBusy
    {
        get => _isSaving;
        private set
        {
            SetField(ref _isSaving, value);
            SaveCommand.RaiseCanExecuteChanged();
        }
    }

    public AsyncRelayCommand SaveCommand { get; }

    public RelayCommand CancelCommand { get; }

    private bool CanSave =>
        !string.IsNullOrEmpty(CurrentPassword) && !string.IsNullOrEmpty(NewPassword);

    private async Task SaveAsync(object? parameter)
    {
        FormMessage = null;
        IsBusy = true;
        try
        {
            await _connection.ChangePasswordAsync(CurrentPassword, NewPassword).ConfigureAwait(true);
            Close();
        }
        catch (ApiRequestException failure)
        {
            ShowPolicyErrors(failure.Error);
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            FormMessage = "The password could not be changed. Try again.";
        }
        finally
        {
            // The passwords live exactly as long as the dialog needs them; never stored.
            // Cleared through the fields, not the setters: the setters' "I was typed"
            // side effect must not wipe the fresh field messages from a failed save.
            _currentPassword = string.Empty;

            _newPassword = string.Empty;
            OnPropertyChanged(nameof(CurrentPassword));
            OnPropertyChanged(nameof(NewPassword));
            SaveCommand.RaiseCanExecuteChanged();
            IsBusy = false;
        }
    }

    private void ShowPolicyErrors(ApiError error)
    {
        FormMessage = error.Message;
        foreach (var field in error.FieldErrors)
        {
            switch (field.Field)
            {
                case "current_password":
                    CurrentFieldMessage = field.Message;
                    break;
                case "new_password":
                    NewFieldMessage = field.Message;
                    break;
            }
        }
    }

    private void Close()
    {
        CurrentPassword = string.Empty;
        NewPassword = string.Empty;
        CloseRequested?.Invoke(this, EventArgs.Empty);
    }
}