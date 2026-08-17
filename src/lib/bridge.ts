import { Channel, invoke, isTauri } from "@tauri-apps/api/core"

import type {
  AccountKey,
  AppSnapshot,
  LoginIntent,
  LoginCompletionResult,
  LoginCancellationStatus,
  OperationEvent,
} from "@/lib/types"

type EventHandler = (event: OperationEvent) => void

function serializeLoginIntent(intent: LoginIntent): LoginIntent {
  return {
    kind: "reauthenticate",
    accountKey: {
      environment: intent.accountKey.environment,
      accountId: intent.accountKey.accountId,
    },
  }
}

function requireDesktop() {
  if (!isTauri()) {
    throw new Error("战网切号器只能在桌面应用中运行")
  }
}

async function call<T>(command: string, args: Record<string, unknown> = {}) {
  requireDesktop()
  return invoke<T>(command, args)
}

async function callWithEvents<T = AppSnapshot>(
  command: string,
  args: Record<string, unknown>,
  onEvent: EventHandler
) {
  requireDesktop()
  const channel = new Channel<OperationEvent>()
  channel.onmessage = onEvent
  return invoke<T>(command, { ...args, onEvent: channel })
}

export const appBridge = {
  load: (onEvent: EventHandler) =>
    callWithEvents("get_app_snapshot", {}, onEvent),
  refresh: () => call<AppSnapshot>("refresh_accounts"),
  switchAccount: (accountKey: AccountKey, onEvent: EventHandler) =>
    callWithEvents("switch_account", { accountKey }, onEvent),
  beginLogin: (intent: LoginIntent, onEvent: EventHandler) =>
    callWithEvents(
      "begin_login",
      { intent: serializeLoginIntent(intent) },
      onEvent
    ),
  completeLogin: (sessionId: string, onEvent: EventHandler) =>
    callWithEvents<LoginCompletionResult>(
      "complete_login",
      { sessionId },
      onEvent
    ),
  requestLoginCancellation: (sessionId: string) =>
    call<LoginCancellationStatus>("request_login_cancellation", { sessionId }),
  cancelLogin: (sessionId: string, onEvent: EventHandler) =>
    callWithEvents("cancel_login", { sessionId }, onEvent),
  removeAccount: (accountKey: AccountKey) =>
    call<AppSnapshot>("remove_account", { accountKey }),
  setClientPath: (executablePath: string) =>
    call<AppSnapshot>("set_client_path", { executablePath }),
  openClient: () => call<AppSnapshot>("open_client"),
  pickClientExecutable: async (
    currentPath: string,
    platform: AppSnapshot["platform"]
  ) => {
    requireDesktop()
    const { open } = await import("@tauri-apps/plugin-dialog")
    const isMac = platform === "macos"
    return open({
      title: "战网切号器",
      defaultPath: currentPath || undefined,
      directory: false,
      multiple: false,
      canCreateDirectories: isMac ? false : undefined,
      filters: [
        {
          name: "Battle.net",
          extensions: [isMac ? "app" : "exe"],
        },
      ],
    })
  },
  confirm: async (
    message: string,
    options: {
      kind?: "info" | "warning" | "error"
      okLabel?: string
    }
  ) => {
    requireDesktop()
    const { confirm } = await import("@tauri-apps/plugin-dialog")
    return confirm(message, {
      title: "战网切号器",
      kind: options.kind,
      okLabel: options.okLabel ?? "继续",
      cancelLabel: "取消",
    })
  },
}
