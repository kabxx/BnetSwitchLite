import * as React from "react"
import { isTauri } from "@tauri-apps/api/core"
import { getCurrentWindow } from "@tauri-apps/api/window"
import { toast } from "sonner"

import { appBridge } from "@/lib/bridge"
import { ATTENTION_TOAST_DURATION, createToastOptions } from "@/lib/toast"
import { userErrorMessage } from "@/lib/user-error"
import type {
  AccountKey,
  AppSnapshot,
  LoginIntent,
  OperationEvent,
  OperationKind,
} from "@/lib/types"

interface ActionOptions {
  kind: OperationKind
  startTitle: string
  startDetail?: string
  startProgress?: number
  successMessage?: string | ((snapshot: AppSnapshot) => string | undefined)
  reloadAfterError?: boolean
  keepOperationOnSuccess?: (snapshot: AppSnapshot) => boolean
  preserveOperationOnStart?: boolean
  task: (onEvent: (event: OperationEvent) => void) => Promise<AppSnapshot>
}

interface QuickActionOptions {
  loadingMessage?: string
  successMessage?: string | ((snapshot: AppSnapshot) => string)
  task: () => Promise<AppSnapshot>
}

interface ActiveLoginCompletion {
  sessionId: string
  task: Promise<AppSnapshot | null> | null
  settled: boolean
  cancelled: boolean
  cancellationTask: Promise<AppSnapshot | null> | null
}

const LOGIN_CANCELLATION_RETRY_COUNT = 10
const LOGIN_CANCELLATION_RETRY_DELAY_MS = 20
const SLOW_DISK_OPERATION_DELAY_MS = 5_000

const DISK_OPERATION_PHASES = new Set([
  "validating",
  "preparing",
  "clearingPointer",
  "restoring",
  "capturing",
  "rollingBack",
])

type LoginCancellationUiState = "idle" | "requesting" | "accepted" | "tooLate"

function wait(milliseconds: number) {
  return new Promise<void>((resolve) =>
    window.setTimeout(resolve, milliseconds)
  )
}

function isCancellationProgress(operation: OperationEvent | null) {
  return (
    operation?.kind === "login" &&
    (operation.phase === "rollingBack" ||
      operation.title.includes("取消") ||
      operation.detail.includes("恢复登录前"))
  )
}

function operationForDisplay(
  operation: OperationEvent | null,
  cancellationState: LoginCancellationUiState
) {
  if (
    cancellationState === "idle" ||
    cancellationState === "tooLate" ||
    isCancellationProgress(operation)
  ) {
    return operation
  }

  const requesting = cancellationState === "requesting"
  return {
    kind: "login" as const,
    phase: requesting ? "requestingCancellation" : "awaitingCancellation",
    title: "正在取消登录",
    detail: requesting ? "正在提交取消请求" : "正在准备取消登录",
    progress: Math.max(operation?.progress ?? 0, requesting ? 16 : 24),
  }
}

const INITIAL_LOAD_OPERATION: OperationEvent = {
  kind: "recovery",
  phase: "starting",
  title: "正在检查本地状态",
  detail: "正在确认上次操作是否完整",
  progress: 4,
}

const PROTECTED_SWITCH_PHASES = new Set([
  "restoring",
  "launchingClient",
  "verifying",
  "awaitingUser",
  "rollingBack",
])

const PROTECTED_LOGIN_PHASES = new Set([
  "clearingPointer",
  "awaitingUser",
  "rollingBack",
])

function isConfigurationWriteInProgress(operation: OperationEvent | null) {
  if (!operation) return false

  if (operation.kind === "remove") return true

  if (operation.kind === "switch") {
    return PROTECTED_SWITCH_PHASES.has(operation.phase)
  }
  if (operation.kind === "login") {
    return PROTECTED_LOGIN_PHASES.has(operation.phase)
  }
  return operation.kind === "recovery" && operation.phase === "rollingBack"
}

function diskOperationKey(operation: OperationEvent | null) {
  if (!operation) return null
  if (
    operation.kind !== "remove" &&
    !DISK_OPERATION_PHASES.has(operation.phase)
  ) {
    return null
  }
  return `${operation.kind}:${operation.phase}`
}

export function useAppController() {
  const [snapshot, setSnapshot] = React.useState<AppSnapshot | null>(null)
  const [loading, setLoading] = React.useState(true)
  const [loadError, setLoadError] = React.useState<string | null>(null)
  const [operation, setOperation] = React.useState<OperationEvent | null>(
    INITIAL_LOAD_OPERATION
  )
  const [quickBusy, setQuickBusy] = React.useState(false)
  const [slowDiskOperation, setSlowDiskOperation] =
    React.useState<OperationEvent | null>(null)
  const [loginCompletionSessionId, setLoginCompletionSessionId] =
    React.useState<string | null>(null)
  const [loginCancellationState, setLoginCancellationState] =
    React.useState<LoginCancellationUiState>("idle")
  const [actionUnlockVersion, setActionUnlockVersion] = React.useState(0)
  const actionLock = React.useRef(true)
  const closeGuardRef = React.useRef(false)
  const closingRef = React.useRef(false)
  const operationRef = React.useRef<OperationEvent | null>(
    INITIAL_LOAD_OPERATION
  )
  const snapshotRef = React.useRef<AppSnapshot | null>(null)
  const closeCancellationRef = React.useRef<Promise<void> | null>(null)
  const loginCompletionRef = React.useRef<ActiveLoginCompletion | null>(null)
  const autoCompletionSessionRef = React.useRef<string | null>(null)

  const setCurrentSnapshot = React.useCallback(
    (nextSnapshot: AppSnapshot | null) => {
      snapshotRef.current = nextSnapshot
      setSnapshot(nextSnapshot)
    },
    []
  )

  const setCurrentOperation = React.useCallback(
    (nextOperation: OperationEvent | null) => {
      operationRef.current = nextOperation
      setOperation(nextOperation)
    },
    []
  )

  const updateOperation = React.useCallback(
    (event: OperationEvent) => {
      const current = operationRef.current
      if (
        event.kind === "login" &&
        current?.kind === "login" &&
        event.progress < current.progress
      ) {
        return
      }

      closeGuardRef.current = isConfigurationWriteInProgress(event)
      setCurrentOperation(event)
    },
    [setCurrentOperation]
  )

  const load = React.useCallback(async () => {
    if (actionLock.current) return
    actionLock.current = true
    closeGuardRef.current = false
    setLoading(true)
    setLoadError(null)
    setCurrentOperation(INITIAL_LOAD_OPERATION)
    try {
      const nextSnapshot = await appBridge.load(updateOperation)
      setCurrentSnapshot(nextSnapshot)
      setLoadError(null)
    } catch (error: unknown) {
      setCurrentSnapshot(null)
      setLoadError(userErrorMessage(error))
    } finally {
      setCurrentOperation(null)
      closeGuardRef.current = false
      actionLock.current = false
      setLoading(false)
    }
  }, [setCurrentOperation, setCurrentSnapshot, updateOperation])

  React.useEffect(() => {
    let cancelled = false

    void appBridge
      .load((event) => {
        if (!cancelled) updateOperation(event)
      })
      .then((nextSnapshot) => {
        if (cancelled) return
        setCurrentSnapshot(nextSnapshot)
        setLoadError(null)
      })
      .catch((error: unknown) => {
        if (cancelled) return
        setLoadError(userErrorMessage(error))
      })
      .finally(() => {
        if (!cancelled) {
          setCurrentOperation(null)
          closeGuardRef.current = false
          actionLock.current = false
          setLoading(false)
        }
      })

    return () => {
      cancelled = true
    }
  }, [setCurrentOperation, setCurrentSnapshot, updateOperation])

  React.useEffect(() => {
    if (!diskOperationKey(operation) || !operation) return

    const timeout = window.setTimeout(
      () => setSlowDiskOperation(operation),
      SLOW_DISK_OPERATION_DELAY_MS
    )
    return () => window.clearTimeout(timeout)
  }, [operation])

  const runAction = React.useCallback(
    async (options: ActionOptions) => {
      if (actionLock.current) return null

      actionLock.current = true
      let keepOperation = false
      const previousOperation = operationRef.current
      const initial: OperationEvent =
        options.preserveOperationOnStart &&
        previousOperation?.kind === options.kind
          ? {
              ...previousOperation,
              phase: "starting",
              title: options.startTitle,
              detail: options.startDetail ?? "正在准备",
              progress: Math.max(
                previousOperation.progress,
                options.startProgress ?? 2
              ),
            }
          : {
              kind: options.kind,
              phase: "starting",
              title: options.startTitle,
              detail: options.startDetail ?? "正在准备",
              progress: options.startProgress ?? 2,
            }
      setCurrentOperation(initial)
      closeGuardRef.current = isConfigurationWriteInProgress(initial)

      try {
        const nextSnapshot = await options.task(updateOperation)
        setCurrentSnapshot(nextSnapshot)
        keepOperation = options.keepOperationOnSuccess?.(nextSnapshot) ?? false
        const successMessage =
          typeof options.successMessage === "function"
            ? options.successMessage(nextSnapshot)
            : options.successMessage
        if (successMessage) {
          toast.success(successMessage, createToastOptions())
        }
        return nextSnapshot
      } catch (error: unknown) {
        const closing = closingRef.current
        if (options.reloadAfterError && !closing) {
          try {
            setCurrentSnapshot(await appBridge.load(updateOperation))
          } catch {
            // Keep the original operation error visible. A later reload can
            // retry recovery if refreshing the snapshot also failed.
          }
        }
        if (!closing) {
          toast.error(
            userErrorMessage(error),
            createToastOptions(ATTENTION_TOAST_DURATION)
          )
        }
        return null
      } finally {
        if (!keepOperation) setCurrentOperation(null)
        closeGuardRef.current = false
        actionLock.current = false
        setActionUnlockVersion((version) => version + 1)
      }
    },
    [setCurrentOperation, setCurrentSnapshot, updateOperation]
  )

  const runQuickAction = React.useCallback(
    async (options: QuickActionOptions) => {
      if (actionLock.current) return null

      actionLock.current = true
      closeGuardRef.current = false
      setQuickBusy(true)
      const task = options.task().then((nextSnapshot) => {
        setCurrentSnapshot(nextSnapshot)
        return nextSnapshot
      })

      if (options.loadingMessage) {
        void toast.promise(task, {
          ...createToastOptions(),
          loading: options.loadingMessage,
          success: (nextSnapshot) =>
            typeof options.successMessage === "function"
              ? options.successMessage(nextSnapshot)
              : options.successMessage,
          error: (error) => ({
            message: userErrorMessage(error),
            duration: ATTENTION_TOAST_DURATION,
          }),
        })
      }

      try {
        const nextSnapshot = await task
        if (!options.loadingMessage && options.successMessage) {
          toast.success(
            typeof options.successMessage === "function"
              ? options.successMessage(nextSnapshot)
              : options.successMessage,
            createToastOptions()
          )
        }
        return nextSnapshot
      } catch (error: unknown) {
        if (!options.loadingMessage) {
          toast.error(
            userErrorMessage(error),
            createToastOptions(ATTENTION_TOAST_DURATION)
          )
        }
        return null
      } finally {
        actionLock.current = false
        setQuickBusy(false)
      }
    },
    [setCurrentSnapshot]
  )

  const refresh = React.useCallback(
    () =>
      runQuickAction({
        loadingMessage: "正在刷新账号",
        successMessage: (nextSnapshot) =>
          nextSnapshot.accounts.length === 0 ? "未发现账号" : "账号列表已刷新",
        task: () => appBridge.refresh(),
      }),
    [runQuickAction]
  )

  const openClient = React.useCallback(() => {
    const alreadyRunning = snapshotRef.current?.client.status === "running"
    return runQuickAction({
      loadingMessage: alreadyRunning ? undefined : "正在启动战网",
      successMessage: alreadyRunning ? "战网客户端已启动" : "战网已启动",
      task: () => appBridge.openClient(),
    })
  }, [runQuickAction])

  const switchAccount = React.useCallback(
    (accountKey: AccountKey, battleTag: string) =>
      runAction({
        kind: "switch",
        startTitle: `正在切换到 ${battleTag}`,
        successMessage: `已切换到 ${battleTag}`,
        task: (onEvent) => appBridge.switchAccount(accountKey, onEvent),
      }),
    [runAction]
  )

  const beginLogin = React.useCallback(
    (intent: LoginIntent) =>
      runAction({
        kind: "login",
        startTitle: "正在准备重新登录",
        keepOperationOnSuccess: (nextSnapshot) =>
          nextSnapshot.loginSession !== null,
        task: (onEvent) => appBridge.beginLogin(intent, onEvent),
      }),
    [runAction]
  )

  const completeLogin = React.useCallback(
    async (sessionId: string) => {
      const existing = loginCompletionRef.current
      if (existing?.sessionId === sessionId && existing.task) {
        return existing.task
      }

      const active: ActiveLoginCompletion = {
        sessionId,
        task: null,
        settled: false,
        cancelled: false,
        cancellationTask: null,
      }
      loginCompletionRef.current = active
      setLoginCompletionSessionId(sessionId)
      setLoginCancellationState("idle")

      const task = runAction({
        kind: "login",
        startTitle: "正在等待登录",
        startDetail: "正在等待战网登录",
        startProgress: 32,
        preserveOperationOnStart: true,
        successMessage: () =>
          active.cancelled ? "已恢复登录前的战网配置" : "账号登录状态已保存",
        reloadAfterError: true,
        task: async (onEvent) => {
          const result = await appBridge.completeLogin(sessionId, onEvent)
          active.cancelled = result.cancelled
          return result.snapshot
        },
      }).finally(() => {
        active.settled = true
      })
      active.task = task

      try {
        return await task
      } finally {
        if (loginCompletionRef.current === active) {
          loginCompletionRef.current = null
          setLoginCompletionSessionId(null)
          setLoginCancellationState("idle")
        }
      }
    },
    [runAction]
  )

  const cancelLogin = React.useCallback(
    (sessionId: string) => {
      const active = loginCompletionRef.current
      if (active?.sessionId === sessionId) {
        if (active.cancellationTask) return active.cancellationTask

        const cancellationTask = (async () => {
          setLoginCancellationState("requesting")
          try {
            for (
              let attempt = 0;
              attempt < LOGIN_CANCELLATION_RETRY_COUNT;
              attempt += 1
            ) {
              const status = await appBridge.requestLoginCancellation(sessionId)
              if (status === "accepted") {
                setLoginCancellationState("accepted")
                return active.task ? await active.task : null
              }
              if (status === "tooLate") {
                setLoginCancellationState("tooLate")
                return active.task ? await active.task : null
              }
              if (active.settled) {
                return active.task ? await active.task : null
              }
              await wait(LOGIN_CANCELLATION_RETRY_DELAY_MS)
            }

            active.cancellationTask = null
            setLoginCancellationState("idle")
            return null
          } catch (error: unknown) {
            active.cancellationTask = null
            setLoginCancellationState("idle")
            toast.error(
              userErrorMessage(error),
              createToastOptions(ATTENTION_TOAST_DURATION)
            )
            return null
          }
        })()
        active.cancellationTask = cancellationTask
        return cancellationTask
      }

      setLoginCancellationState("accepted")
      return runAction({
        kind: "login",
        startTitle: "正在取消登录",
        startDetail: "正在提交取消请求",
        startProgress: 32,
        successMessage: "已恢复登录前的战网配置",
        task: (onEvent) => appBridge.cancelLogin(sessionId, onEvent),
      }).finally(() => setLoginCancellationState("idle"))
    },
    [runAction]
  )

  const removeAccount = React.useCallback(
    (accountKey: AccountKey, battleTag: string) =>
      runAction({
        kind: "remove",
        startTitle: `正在移除 ${battleTag}`,
        startDetail: "正在删除本地登录状态",
        successMessage: `已移除 ${battleTag}`,
        task: () => appBridge.removeAccount(accountKey),
      }),
    [runAction]
  )

  const setClientPath = React.useCallback(
    (executablePath: string) =>
      runQuickAction({
        successMessage: "客户端路径已更新",
        task: () => appBridge.setClientPath(executablePath),
      }),
    [runQuickAction]
  )

  const busy = operation !== null || quickBusy

  React.useEffect(() => {
    const session = snapshot?.loginSession
    if (!session) {
      autoCompletionSessionRef.current = null
      return
    }
    if (
      loading ||
      actionLock.current ||
      autoCompletionSessionRef.current === session.id
    ) {
      return
    }

    autoCompletionSessionRef.current = session.id
    void completeLogin(session.id)
  }, [actionUnlockVersion, completeLogin, loading, snapshot])

  React.useEffect(() => {
    if (!isTauri()) return

    let disposed = false
    let unlisten: (() => void) | undefined
    const appWindow = getCurrentWindow()

    void appWindow
      .onCloseRequested(async (event) => {
        const activeSnapshot = snapshotRef.current
        const pendingLogin = activeSnapshot?.loginSession

        if (!pendingLogin) {
          if (closeGuardRef.current) {
            event.preventDefault()
            toast.info(
              "正在保护战网配置，请等待当前步骤完成",
              createToastOptions()
            )
          }
          return
        }

        event.preventDefault()
        if (closeCancellationRef.current) {
          toast.info("正在恢复战网配置，请稍候", createToastOptions())
          return
        }

        closingRef.current = true
        const cancellation = (async () => {
          try {
            await cancelLogin(pendingLogin.id)
          } finally {
            // A failed rollback leaves its durable recovery record in place;
            // the next launch can retry it. Closing the window must not turn
            // that recoverable state into an unclosable error loop.
            await appWindow.destroy()
          }
        })().finally(() => {
          closeCancellationRef.current = null
        })

        closeCancellationRef.current = cancellation
        await cancellation
      })
      .then((stop) => {
        if (disposed) stop()
        else unlisten = stop
      })

    return () => {
      disposed = true
      unlisten?.()
    }
  }, [cancelLogin])

  const canCancelLogin =
    snapshot?.loginSession != null &&
    (!busy ||
      (loginCompletionSessionId === snapshot?.loginSession?.id &&
        loginCancellationState === "idle"))

  const displayedOperation = operationForDisplay(
    operation,
    loginCancellationState
  )
  const displayedOperationWithSlowDiskNotice =
    displayedOperation === operation &&
    slowDiskOperation === operation &&
    displayedOperation
      ? {
          ...displayedOperation,
          detail: "磁盘操作耗时较长，请稍候",
        }
      : displayedOperation

  return {
    snapshot,
    loading,
    loadError,
    operation: displayedOperationWithSlowDiskNotice,
    busy,
    canCancelLogin,
    reload: load,
    refresh,
    switchAccount,
    beginLogin,
    cancelLogin,
    openClient,
    removeAccount,
    setClientPath,
  }
}
