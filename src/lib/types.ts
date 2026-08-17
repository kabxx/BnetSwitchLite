export type SnapshotStatus = "ready" | "expired" | "missing"
export type ClientStatus = "running" | "stopped"

export interface AccountKey {
  environment: string
  accountId: string
}

export interface AccountSnapshot {
  key: AccountKey
  id: string
  battleTag: string
  region: string
  environment: string
  snapshotStatus: SnapshotStatus
  lastSavedAt: number | null
  note: string | null
}

export interface ClientSnapshot {
  status: ClientStatus
  executablePath: string
  detectedAutomatically: boolean
}

export type LoginIntent = {
  kind: "reauthenticate"
  accountKey: AccountKey
}

export interface LoginSessionSnapshot {
  id: string
  intent: LoginIntent
  createdAt: number
}

export interface LoginCompletionResult {
  snapshot: AppSnapshot
  cancelled: boolean
}

export type LoginCancellationStatus = "accepted" | "starting" | "tooLate"

export interface AppSnapshot {
  appName: string
  version: string
  mode: "desktop"
  platform: "windows" | "macos"
  dataDirectory: string
  client: ClientSnapshot
  accounts: AccountSnapshot[]
  loginSession: LoginSessionSnapshot | null
  notice: string | null
  updatedAt: number
}

export type OperationKind =
  | "recovery"
  | "refresh"
  | "switch"
  | "login"
  | "client"
  | "configure"
  | "remove"

export interface OperationEvent {
  kind: OperationKind
  phase: string
  title: string
  detail: string
  progress: number
}

export function accountKeyId(key: AccountKey) {
  return `${key.environment}:${key.accountId}`
}

export function accountKeysEqual(
  first: AccountKey | null,
  second: AccountKey | null
) {
  if (!first || !second) return first === second
  return (
    first.environment === second.environment &&
    first.accountId === second.accountId
  )
}
