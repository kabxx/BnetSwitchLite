import * as React from "react"
import { isTauri } from "@tauri-apps/api/core"
import { LogicalSize, PhysicalPosition } from "@tauri-apps/api/dpi"
import { getCurrentWindow } from "@tauri-apps/api/window"
import { toast } from "sonner"

import { AccountList } from "@/components/account-list"
import { AppErrorState, AppSkeleton } from "@/components/app-states"
import { Toaster } from "@/components/ui/sonner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { useAppController } from "@/hooks/use-app-controller"
import { appBridge } from "@/lib/bridge"
import { ATTENTION_TOAST_DURATION, createToastOptions } from "@/lib/toast"
import type { AccountSnapshot, LoginIntent } from "@/lib/types"
import { accountKeyId } from "@/lib/types"
import { userErrorMessage } from "@/lib/user-error"

const MIN_WINDOW_HEIGHT = 64
const MAX_WINDOW_HEIGHT = 680
const MIN_WINDOW_WIDTH = 480
const MAX_WINDOW_WIDTH = 1600

function AppProviders({ children }: { children: React.ReactNode }) {
  return (
    <TooltipProvider delay={350}>
      {children}
      <Toaster
        closeButton
        mobileOffset={{ bottom: 52, left: 8, right: 8 }}
        offset={{ bottom: 52, right: 12 }}
        position="bottom-right"
      />
    </TooltipProvider>
  )
}

function useAdaptiveWindow(layoutKey: string | null) {
  const adjustmentQueue = React.useRef<Promise<void>>(Promise.resolve())

  React.useLayoutEffect(() => {
    if (layoutKey === null || !isTauri()) return
    let cancelled = false

    const fit = async () => {
      await document.fonts.ready
      await new Promise<void>((resolve) =>
        requestAnimationFrame(() => resolve())
      )
      await new Promise<void>((resolve) =>
        requestAnimationFrame(() => resolve())
      )
      if (cancelled) return

      const runAdjustment = async () => {
        if (cancelled) return

        const waitForLayout = () =>
          new Promise<void>((resolve) =>
            requestAnimationFrame(() => resolve())
          )

        const appWindow = getCurrentWindow()
        await appWindow.setMinSize(
          new LogicalSize(MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT)
        )
        await appWindow.setMaxSize(
          new LogicalSize(MAX_WINDOW_WIDTH, MAX_WINDOW_HEIGHT)
        )
        if (cancelled) return

        const rows = document.querySelector<HTMLElement>(
          "[data-account-rows-content]"
        )
        const rowsStage = document.querySelector<HTMLElement>(
          "[data-account-rows]"
        )
        const separator = document.querySelector<HTMLElement>(
          "[data-account-separator]"
        )
        const toolbar = document.querySelector<HTMLElement>(
          "[data-account-toolbar]"
        )
        const content = document.querySelector<HTMLElement>(
          "[data-window-content]"
        )
        const stageStyle = rowsStage ? getComputedStyle(rowsStage) : null
        const stagePadding = stageStyle
          ? (parseFloat(stageStyle.paddingTop) || 0) +
            (parseFloat(stageStyle.paddingBottom) || 0)
          : 0
        const rowsContentStyle = rows ? getComputedStyle(rows) : null
        const rowsContentBorderTop = rowsContentStyle
          ? parseFloat(rowsContentStyle.borderTopWidth) || 0
          : 0
        const rowsContentBorderBottom = rowsContentStyle
          ? parseFloat(rowsContentStyle.borderBottomWidth) || 0
          : 0
        const accountRows = rows
          ? Array.from(
              rows.querySelectorAll<HTMLElement>("[data-account-row-item]")
            )
          : []
        const rowsContentRect = rows?.getBoundingClientRect()
        // A card can already be clipped by the initial native window height.
        // Use its scroll height and rendered row bottoms as intrinsic signals;
        // the visible rect alone would preserve that clipped height forever.
        const rowsScrollHeight = rows
          ? rows.scrollHeight + rowsContentBorderTop + rowsContentBorderBottom
          : 0
        const measuredRowsBottom =
          rowsContentRect && accountRows.length > 0
            ? Math.max(
                ...accountRows.map(
                  (row) => row.getBoundingClientRect().bottom - rowsContentRect.top
                )
              ) + rowsContentBorderBottom
            : 0
        const rowsNaturalHeight = Math.ceil(
          Math.max(
            rowsContentRect?.height ?? 0,
            rowsScrollHeight,
            measuredRowsBottom
          )
        )
        const naturalHeight =
          rows && separator && toolbar
            ? Math.ceil(
                rowsNaturalHeight +
                  stagePadding +
                  separator.offsetHeight +
                  toolbar.offsetHeight
              )
            : Math.ceil(content?.scrollHeight ?? 0)
        if (naturalHeight === 0) return

        if (rowsStage) {
          rowsStage.style.overflowY =
            naturalHeight > MAX_WINDOW_HEIGHT ? "auto" : "hidden"
        }

        let maximumHeight = MAX_WINDOW_HEIGHT
        if (naturalHeight > MAX_WINDOW_HEIGHT && accountRows.length > 0) {
          const fixedHeight =
            stagePadding + separator!.offsetHeight + toolbar!.offsetHeight
          const availableRowsHeight = MAX_WINDOW_HEIGHT - fixedHeight
          const lastCompleteRow = accountRows.findLast(
            (row) => {
              const cardTop = rowsContentRect?.top ?? 0
              const rowBottom = row.getBoundingClientRect().bottom - cardTop
              return (
                rowBottom + rowsContentBorderBottom <= availableRowsHeight
              )
            }
          )
          if (lastCompleteRow) {
            const cardTop = rowsContentRect?.top ?? 0
            const lastRowBottom =
              lastCompleteRow.getBoundingClientRect().bottom - cardTop
            maximumHeight = Math.ceil(
              fixedHeight +
                lastRowBottom +
                rowsContentBorderBottom
            )
          }
        }
        const targetHeight = Math.max(
          MIN_WINDOW_HEIGHT,
          Math.min(maximumHeight, naturalHeight)
        )
        const minimumHeight =
          naturalHeight <= MAX_WINDOW_HEIGHT
            ? targetHeight
            : MIN_WINDOW_HEIGHT
        const finalMaximumHeight = Math.max(maximumHeight, targetHeight)
        const [innerSize, outerPosition, outerSize, scaleFactor] =
          await Promise.all([
            appWindow.innerSize(),
            appWindow.outerPosition(),
            appWindow.outerSize(),
            appWindow.scaleFactor(),
          ])
        if (cancelled) return
        const width = innerSize.width / scaleFactor
        const currentHeight = innerSize.height / scaleFactor

        await appWindow.setMaxSize(
          new LogicalSize(MAX_WINDOW_WIDTH, finalMaximumHeight)
        )
        // When the complete list fits below the cap, do not let native window
        // restoration compress it into a partial row with a scrollbar.
        await appWindow.setMinSize(
          new LogicalSize(MIN_WINDOW_WIDTH, minimumHeight)
        )

        let appliedTargetHeight = targetHeight
        const readViewportHeight = () =>
          Math.max(window.innerHeight, document.documentElement.clientHeight)

        const resizeAndWait = async (height: number) => {
          await appWindow.setSize(new LogicalSize(width, height))
          await waitForLayout()
          await waitForLayout()
        }

        if (Math.abs(currentHeight - appliedTargetHeight) > 1) {
          const previousCenter = {
            x: outerPosition.x + outerSize.width / 2,
            y: outerPosition.y + outerSize.height / 2,
          }
          await resizeAndWait(appliedTargetHeight)

          const resizedOuterSize = await appWindow.outerSize()

          const nextX = Math.round(
            previousCenter.x - resizedOuterSize.width / 2
          )
          const nextY = Math.round(
            previousCenter.y - resizedOuterSize.height / 2
          )
          await appWindow.setPosition(new PhysicalPosition(nextX, nextY))
        }

        // macOS can report the requested logical size while its WebView still
        // has less usable height than the window frame. Measure the viewport,
        // not the root element's CSS height, and compensate for that deficit.
        if (naturalHeight <= MAX_WINDOW_HEIGHT) {
          const layoutDeficit = naturalHeight - readViewportHeight()
          if (layoutDeficit > 1) {
            appliedTargetHeight = Math.min(
              MAX_WINDOW_HEIGHT,
              appliedTargetHeight + Math.ceil(layoutDeficit)
            )
            await appWindow.setMinSize(
              new LogicalSize(MIN_WINDOW_WIDTH, appliedTargetHeight)
            )
            await appWindow.setMaxSize(
              new LogicalSize(
                MAX_WINDOW_WIDTH,
                Math.max(maximumHeight, appliedTargetHeight)
              )
            )
            await resizeAndWait(appliedTargetHeight)
          }
        }
      }

      const queuedAdjustment = adjustmentQueue.current.then(
        runAdjustment,
        runAdjustment
      )
      adjustmentQueue.current = queuedAdjustment
      await queuedAdjustment
      if (!cancelled) {
        await getCurrentWindow().show()
      }
    }

    void fit().catch((error: unknown) => {
      console.error("无法自适应窗口尺寸", error)
    })
    return () => {
      cancelled = true
    }
  }, [layoutKey])
}

export function App() {
  const controller = useAppController()
  const snapshot = controller.snapshot
  const shownNoticeRef = React.useRef<string | null>(null)
  const layoutKey = snapshot
    ? `accounts:${snapshot.accounts.map((account) => accountKeyId(account.key)).join("|")}`
    : controller.loading
      ? "status:loading"
      : "status:error"
  useAdaptiveWindow(layoutKey)

  React.useEffect(() => {
    const notice = snapshot?.notice
    if (
      !notice ||
      snapshot.accounts.length === 0 ||
      shownNoticeRef.current === notice
    ) {
      return
    }
    shownNoticeRef.current = notice
    toast.info(notice, createToastOptions())
  }, [snapshot?.notice, snapshot?.accounts.length])

  if (controller.loading) {
    return (
      <AppProviders>
        <AppSkeleton operation={controller.operation} />
      </AppProviders>
    )
  }

  if (!snapshot) {
    return (
      <AppProviders>
        <AppErrorState
          message={controller.loadError ?? "请重新启动战网切号器"}
          onRetry={controller.reload}
        />
      </AppProviders>
    )
  }

  const configureClient = async () => {
    try {
      const selected = await appBridge.pickClientExecutable(
        snapshot.client.executablePath,
        snapshot.platform
      )
      if (typeof selected !== "string") return false
      return (await controller.setClientPath(selected)) !== null
    } catch (error: unknown) {
      toast.error(userErrorMessage(error, "无法打开系统文件选择器"), {
        ...createToastOptions(ATTENTION_TOAST_DURATION),
      })
      return false
    }
  }

  const ensureClient = async () => {
    if (snapshot.client.executablePath) return true
    toast.error(userErrorMessage("请选择有效的 Battle.net 客户端"), {
      ...createToastOptions(ATTENTION_TOAST_DURATION),
    })
    return false
  }

  const confirmSwitch = async (account: AccountSnapshot) => {
    if (!(await ensureClient())) return
    const message =
      snapshot.client.status === "running"
        ? `战网客户端会安全退出并切换到 ${account.battleTag}。`
        : `将启动战网并切换到 ${account.battleTag}。`
    const confirmed = await appBridge.confirm(message, {
      okLabel: "切换",
    })
    if (confirmed) {
      void controller.switchAccount(account.key, account.battleTag)
    }
  }

  const refreshAccounts = async () => {
    if (!(await ensureClient())) return
    await controller.refresh()
  }

  const confirmRemove = async (account: AccountSnapshot) => {
    const confirmed = await appBridge.confirm(
      `将从战网切号器中移除 ${account.battleTag}，并删除本地保存的登录状态。`,
      {
        kind: "warning",
        okLabel: "移除",
      }
    )
    if (confirmed) {
      void controller.removeAccount(account.key, account.battleTag)
    }
  }

  const beginLogin = async (intent: LoginIntent) => {
    if (!(await ensureClient())) return
    const message =
      snapshot.client.status === "running"
        ? "战网客户端会安全退出，然后打开登录界面。"
        : "将启动战网登录界面。"
    const confirmed = await appBridge.confirm(message, {
      okLabel: "开始登录",
    })
    if (confirmed) void controller.beginLogin(intent)
  }

  const session = snapshot.loginSession

  return (
    <AppProviders>
      <div
        className="app-shell flex h-svh flex-col bg-card text-foreground"
        data-window-content
      >
        <main className="flex min-h-0 flex-1">
          <AccountList
            accounts={snapshot.accounts}
            busy={controller.busy}
            canCancelLogin={controller.canCancelLogin}
            loginSession={session}
            onCancelLogin={() => {
              if (session) void controller.cancelLogin(session.id)
            }}
            onConfigurePath={() => void configureClient()}
            onOpenClient={() => void controller.openClient()}
            onDelete={(account) => void confirmRemove(account)}
            onRefresh={() => void refreshAccounts()}
            onRelogin={(account) =>
              void beginLogin({
                kind: "reauthenticate",
                accountKey: account.key,
              })
            }
            onSwitch={(account) => void confirmSwitch(account)}
            operation={controller.operation}
          />
        </main>
      </div>
    </AppProviders>
  )
}

export default App
