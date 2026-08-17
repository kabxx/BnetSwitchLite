export const DEFAULT_TOAST_DURATION = 1_000
export const ATTENTION_TOAST_DURATION = 1_500

const TOAST_INSTANCE_ID = Date.now().toString(36)
let toastSequence = 0

export function createToastOptions(duration: number = DEFAULT_TOAST_DURATION) {
  toastSequence += 1
  return {
    duration,
    id: `bnetswitchlite-${TOAST_INSTANCE_ID}-${toastSequence}`,
  }
}
