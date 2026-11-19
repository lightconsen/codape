import { Capabilities } from "@/components/sections/capabilities"
import { Hero } from "@/components/sections/hero"
import { Install } from "@/components/sections/install"
import { SiteFooter } from "@/components/sections/site-footer"
import { SiteHeader } from "@/components/site/site-header"

export default function App() {
  return (
    <>
      <a
        href="#main"
        className="sr-only focus:not-sr-only focus:fixed focus:top-3 focus:left-3 focus:z-[100] focus:border-2 focus:border-primary-foreground focus:bg-primary focus:px-3 focus:py-2 focus:font-mono focus:text-[12px] focus:text-primary-foreground"
      >
        Skip to content
      </a>
      <SiteHeader />
      <main id="main">
        <Hero />
        <Capabilities />
        <Install />
      </main>
      <SiteFooter />
    </>
  )
}
