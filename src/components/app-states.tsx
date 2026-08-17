import { CircleAlert, RefreshCw } from "lucide-react"
import { OperationBar } from "@/components/operation-bar"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import type { OperationEvent } from "@/lib/types"

export function AppSkeleton({
  operation,
}: {
  operation: OperationEvent | null
}) {
  return (
    <div className="flex h-svh flex-col bg-background" data-window-content>
      <div className="flex min-h-0 w-full flex-1 flex-col gap-px bg-border">
        {[0, 1].map((item) => (
          <Skeleton key={item} className="h-16 w-full rounded-none bg-card" />
        ))}
      </div>
      <div className="relative grid min-h-10 shrink-0 grid-cols-[1fr_auto_1fr] items-center bg-muted/35">
        <OperationBar operation={operation} />
      </div>
    </div>
  )
}

export function AppErrorState({
  message,
  onRetry,
}: {
  message: string
  onRetry: () => void
}) {
  return (
    <section
      aria-label="应用状态错误"
      aria-live="assertive"
      className="flex h-16 min-h-16 items-center justify-between gap-3 overflow-hidden bg-card px-2.5"
      data-window-content
      role="alert"
    >
      <div className="flex min-w-0 items-center gap-2.5">
        <span className="grid size-[17px] shrink-0 place-items-center text-muted-foreground">
          <CircleAlert aria-hidden="true" className="size-[17px]" />
        </span>
        <div className="flex min-w-0 items-baseline gap-2 overflow-hidden whitespace-nowrap">
          <strong className="shrink-0 truncate font-heading text-sm font-semibold">
            状态读取失败
          </strong>
          <span
            className="min-w-0 truncate text-[13px] font-medium text-muted-foreground"
            title={message}
          >
            {message}
          </span>
        </div>
      </div>
      <Button onClick={onRetry} size="sm" variant="quiet">
        <RefreshCw data-icon="inline-start" />
        重试
      </Button>
    </section>
  )
}
