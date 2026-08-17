export function formatRelativeTime(timestamp: number | null) {
  if (timestamp === null) return "尚未保存"

  const elapsed = Math.max(0, Date.now() - timestamp)
  const minutes = Math.floor(elapsed / 60_000)

  if (minutes < 1) return "刚刚"
  if (minutes < 60) return `${minutes} 分钟前`

  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`

  const days = Math.floor(hours / 24)
  if (days < 30) return `${days} 天前`

  return new Intl.DateTimeFormat("zh-CN", {
    month: "short",
    day: "numeric",
  }).format(timestamp)
}

export function accountInitials(battleTag: string) {
  const characters = Array.from(battleTag.split("#")[0].trim())
  const length = characters.some((character) =>
    /\p{Script=Han}/u.test(character)
  )
    ? 1
    : 2
  return characters.slice(0, length).join("").toUpperCase()
}

export type AccountAvatarTone = "mint" | "cyan" | "violet" | "rose"

export function accountAvatarTone(accountId: string): AccountAvatarTone {
  const tones: AccountAvatarTone[] = ["mint", "cyan", "violet", "rose"]
  const hash = Array.from(accountId).reduce(
    (value, character) => (value * 31 + character.codePointAt(0)!) >>> 0,
    0
  )
  return tones[hash % tones.length]
}
