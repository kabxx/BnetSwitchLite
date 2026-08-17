const DEFAULT_ERROR_MESSAGE = "操作没有完成，请稍后重试"
const MAX_MESSAGE_LENGTH = 56

const WRAPPER_PREFIX =
  /^(?:(?:切换)?事务失败|登录流程失败|操作失败|账号快照不可用|应用数据目录不可用|恢复数据不可用)\s*[：:]\s*/i

const USER_ERROR_RULES: ReadonlyArray<readonly [RegExp, string]> = [
  [
    /登录账号.*(?:所选|目标|对应).*环境.*不一致|登录结果.*目标账号.*不一致/,
    "登录结果与目标账号不一致，请确认登录的是所选账号",
  ],
  [
    /另一个账号操作|已有账号操作|账号操作正在进行|请先完成或取消当前(?:账号|登录)|请先处理后再试/,
    "请先完成当前账号操作",
  ],
  [
    /自动恢复未完成|恢复(?:原配置|登录前配置|战网配置)?失败|无法(?:完成|继续)?恢复|恢复材料已保留|还原配置.*失败/,
    "战网配置恢复失败，请重新打开本工具",
  ],
  [
    /未能在限定时间内正常退出|客户端仍在运行|无法确认.*客户端.*退出|请先退出.*客户端/,
    "请先退出 Battle.net 客户端后重试",
  ],
  [
    /请选择有效的 Battle\.net 客户端|找不到 Battle\.net 客户端|未找到 Battle\.net\.exe|客户端(?:路径|签名).*无效/,
    "请选择有效的 Battle.net 客户端",
  ],
  [
    /无法启动 Battle\.net|启动 Battle\.net.*失败|未能启动.*客户端|无法重新启动战网/,
    "无法启动 Battle.net 客户端",
  ],
  [
    /未找到 Battle\.net 本地账号数据|未检测到.*账号|尚未检测到已登录账号/,
    "未检测到本地战网账号",
  ],
  [
    /账号缓存格式不受支持|无法读取 Battle\.net 账号缓存|无法读取.*账号数据|CachedData\.db/i,
    "无法读取战网账号数据",
  ],
  [
    /无法(?:删除|清理|隔离).*账号快照|无法清理敏感目录/,
    "无法删除本地登录状态，请稍后重试",
  ],
  [
    /账号快照.*(?:不可用|失效|损坏|校验失败|不存在)|登录状态.*(?:失效|缺失)|认证快照.*(?:不一致|不可用|没有)/,
    "账号登录状态不可用，请重新登录",
  ],
  [
    /应用数据目录|便携数据目录|拒绝访问|权限|磁盘空间|not enough space|no space left|access is denied/i,
    "本地数据不可用，请检查目录权限和磁盘空间",
  ],
  [/无法读取系统目录/, "无法访问系统目录"],
  [
    /操作状态不可用|应用状态.*异常|状态不可用.*重启/,
    "应用状态异常，请重新启动",
  ],
  [
    /登录会话.*失效|待确认切换.*失效|没有待(?:确认|取消)|当前.*不能(?:提交|继续)/,
    "当前操作已经失效，请重新开始",
  ],
  [
    /账号.*不一致|登录账号.*不正确|检测到的账号.*不一致/,
    "当前登录账号不正确，请切换后重试",
  ],
  [
    /Battle\.net 已退出|战网客户端.*已退出|确认账号前已退出/,
    "Battle.net 已退出，请重新开始操作",
  ],
]

function rawErrorMessage(error: unknown) {
  if (error instanceof Error) return error.message
  if (typeof error === "string") return error
  return ""
}

function removeWrapperPrefixes(message: string) {
  let result = message.trim()
  let previous = ""

  while (result !== previous) {
    previous = result
    result = result.replace(WRAPPER_PREFIX, "").trim()
  }

  return result
}

function conciseFallback(message: string, fallback: string) {
  const firstClause = removeWrapperPrefixes(
    message.replace(/\r?\n/g, " ").split(/[；;]/, 1)[0] ?? ""
  )
    .replace(/[A-Za-z]:\\[^，。；;\s]+/g, "本地文件")
    .replace(/\/(?:Users|home|private|var|tmp)\/[^，。；;\s]+/gi, "本地文件")
    .replace(/\s*\((?:os error|code)\s*\d+\)\s*/gi, "")
    .replace(/\s+/g, " ")
    .trim()

  if (
    !firstClause ||
    !/[\u3400-\u9fff]/u.test(firstClause) ||
    /(?:stack backtrace|caused by|panicked at|\.rs:\d+|^error\b|^err\(|0x[\da-f]+|[\w.-]+::[\w.-]|[{}[\]<>])/i.test(
      firstClause
    )
  ) {
    return fallback
  }

  const separatorIndex = firstClause.search(/[：:]/)
  const leadingClause =
    separatorIndex > 1 ? firstClause.slice(0, separatorIndex).trim() : ""
  const withoutTechnicalDetail =
    leadingClause.length <= 32 &&
    /无法|未能|失败|不可用|无效|不存在/.test(leadingClause)
      ? leadingClause
      : firstClause
  if (withoutTechnicalDetail.length <= MAX_MESSAGE_LENGTH) {
    return withoutTechnicalDetail
  }
  return `${withoutTechnicalDetail.slice(0, MAX_MESSAGE_LENGTH - 1)}…`
}

export function userErrorMessage(
  error: unknown,
  fallback = DEFAULT_ERROR_MESSAGE
) {
  const raw = rawErrorMessage(error).trim()
  if (!raw) return fallback

  const normalized = removeWrapperPrefixes(raw)
  const known = USER_ERROR_RULES.find(
    ([pattern]) => pattern.test(raw) || pattern.test(normalized)
  )
  return known?.[1] ?? conciseFallback(normalized, fallback)
}
