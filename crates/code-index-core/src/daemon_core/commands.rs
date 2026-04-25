// Команды, которыми HTTP-сервер общается с основным циклом демона (runner).
//
// HTTP-handler принимает запрос, отправляет `DaemonCommand` в mpsc-канал и
// ждёт ответ через `oneshot`. Runner-цикл обрабатывает команду, выполняет
// побочные действия (запуск/остановка worker'ов) и отвечает.

use tokio::sync::{mpsc, oneshot};

use super::ipc::{ReloadResponse, StopResponse};

/// Отправитель команд в runner. Клонируется в каждый HTTP-handler.
pub type CommandSender = mpsc::Sender<DaemonCommand>;
/// Приёмник команд у runner'а.
pub type CommandReceiver = mpsc::Receiver<DaemonCommand>;

pub fn channel() -> (CommandSender, CommandReceiver) {
    mpsc::channel(32)
}

/// Команды, отправляемые HTTP-сервером в runner.
pub enum DaemonCommand {
    /// Перечитать конфиг. Runner сравнивает список папок с текущим состоянием,
    /// запускает worker'ы для добавленных и просит завершиться удалённые.
    Reload {
        respond_to: oneshot::Sender<ReloadResponse>,
    },
    /// Полное завершение демона.
    Stop {
        respond_to: oneshot::Sender<StopResponse>,
    },
}
