import type { Viewport } from "next";
import { IBM_Plex_Mono, Manrope, Newsreader } from "next/font/google";

import { SiteFooter } from "@/components/shell/site-footer";
import { SiteHeader } from "@/components/shell/site-header";
import { homeMetadata } from "@/lib/site";

import "./globals.css";

const display = Newsreader({
  subsets: ["latin"],
  weight: "400",
  variable: "--font-display",
  display: "optional",
  preload: false,
});

const body = Manrope({
  subsets: ["latin"],
  variable: "--font-body",
  display: "swap",
  preload: false,
});

const mono = IBM_Plex_Mono({
  subsets: ["latin"],
  weight: ["400", "600"],
  variable: "--font-mono",
  display: "swap",
  preload: false,
});

export const metadata = homeMetadata;

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  themeColor: "#f2efe6",
};

export default function RootLayout({ children }: LayoutProps<"/">) {
  return (
    <html lang="en" className={`${display.variable} ${body.variable} ${mono.variable}`}>
      <body>
        <a className="skip-link" href="#main-content">Skip to content</a>
        <SiteHeader />
        <div id="main-content">{children}</div>
        <SiteFooter />
      </body>
    </html>
  );
}
