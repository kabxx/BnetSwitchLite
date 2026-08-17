import * as React from "react"

const COLOR_SCHEME_QUERY = "(prefers-color-scheme: dark)"

function applySystemTheme(mediaQuery: MediaQueryList) {
  document.documentElement.classList.toggle("dark", mediaQuery.matches)
  document.documentElement.classList.toggle("light", !mediaQuery.matches)
}

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  React.useEffect(() => {
    const mediaQuery = window.matchMedia(COLOR_SCHEME_QUERY)
    const handleChange = () => applySystemTheme(mediaQuery)

    handleChange()
    mediaQuery.addEventListener("change", handleChange)
    return () => mediaQuery.removeEventListener("change", handleChange)
  }, [])

  return children
}
