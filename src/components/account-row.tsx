import { Trash2, X } from "lucide-react"

import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Button } from "@/components/ui/button"
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip"
import {
  accountAvatarTone,
  accountInitials,
  formatRelativeTime,
} from "@/lib/format"
import type { AccountSnapshot } from "@/lib/types"
import { cn } from "@/lib/utils"

interface AccountRowProps {
  account: AccountSnapshot
  busy: boolean
  cancelLoginDisabled: boolean
  loginPending: boolean
  onCancelLogin: () => void
  onSwitch: () => void
  onRelogin: () => void
  onDelete: () => void
}

export function AccountRow({
  account,
  busy,
  cancelLoginDisabled,
  loginPending,
  onCancelLogin,
  onSwitch,
  onRelogin,
  onDelete,
}: AccountRowProps) {
  const actionLabel =
    account.snapshotStatus === "expired" ? "重新登录" : "登录并保存"
  const snapshotDetail =
    account.snapshotStatus === "ready"
      ? `${formatRelativeTime(account.lastSavedAt)}更新`
      : account.snapshotStatus === "expired"
        ? "登录已失效"
        : "尚未保存"

  return (
    <article
      aria-busy={loginPending || undefined}
      className={cn(
        "account-row relative grid min-h-16 items-center gap-3 px-3 py-2.5 transition-colors duration-150 hover:bg-accent/45",
        loginPending && "bg-muted/30 hover:bg-muted/30"
      )}
    >
      <Avatar size="lg">
        <AvatarFallback tone={accountAvatarTone(account.id)}>
          {accountInitials(account.battleTag)}
        </AvatarFallback>
      </Avatar>

      <div className="min-w-0">
        <h2 className="truncate text-sm leading-5 font-semibold">
          {account.battleTag}
        </h2>
        <p className="mt-0.5 flex min-w-0 items-center gap-1.5 truncate text-xs text-muted-foreground">
          <span>{account.region}</span>
          <span aria-hidden="true">·</span>
          <span
            className={cn(
              "truncate",
              account.snapshotStatus === "expired" && "text-destructive",
              account.snapshotStatus === "missing" && "text-warning"
            )}
          >
            {snapshotDetail}
          </span>
        </p>
      </div>

      {loginPending ? (
        <div className="account-row-action flex shrink-0 items-center justify-end">
          <Tooltip>
            <TooltipTrigger
              render={
                <Button
                  aria-label="取消登录并恢复"
                  disabled={cancelLoginDisabled}
                  onClick={onCancelLogin}
                  size="icon-sm"
                  variant="quiet"
                />
              }
            >
              <X />
            </TooltipTrigger>
            <TooltipContent>取消登录并恢复</TooltipContent>
          </Tooltip>
        </div>
      ) : (
        <div className="account-row-action grid shrink-0 grid-cols-[5.75rem_1.75rem] items-center gap-1">
          {account.snapshotStatus === "ready" ? (
            <Button
              className="w-full"
              disabled={busy}
              onClick={onSwitch}
              size="sm"
            >
              切换
            </Button>
          ) : (
            <Button
              className="w-full"
              disabled={busy}
              onClick={onRelogin}
              size="sm"
              variant="surface-outline"
            >
              {actionLabel}
            </Button>
          )}

          <Tooltip>
            <TooltipTrigger
              render={
                <Button
                  aria-label={`移除 ${account.battleTag}`}
                  disabled={busy}
                  onClick={onDelete}
                  size="icon-sm"
                  variant="quiet-destructive"
                />
              }
            >
              <Trash2 />
            </TooltipTrigger>
            <TooltipContent>移除账号</TooltipContent>
          </Tooltip>
        </div>
      )}
    </article>
  )
}
