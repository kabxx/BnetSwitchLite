import { LoaderCircle } from "lucide-react"

import { Progress } from "@/components/ui/progress"
import type { OperationEvent } from "@/lib/types"

export function OperationBar({
  operation,
}: {
  operation: OperationEvent | null
}) {
  if (!operation) return null

  return (
    <div className="col-start-2 flex min-w-0 items-center justify-center gap-1.5 text-muted-foreground">
      <p aria-atomic="true" aria-live="polite" className="sr-only">
        {operation.title}，{operation.detail}
      </p>
      <LoaderCircle className="size-3.5 shrink-0 animate-spin text-primary" />
      <span className="operation-bar-label max-w-44 truncate text-xs">
        {operation.detail}
      </span>
      <Progress
        aria-label={operation.title}
        className="absolute inset-x-0 top-0 h-px rounded-none"
        value={operation.progress}
      />
    </div>
  )
}
