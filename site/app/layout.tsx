import type { Metadata } from "next";
import Link from "next/link";
import "./globals.css";

export const metadata: Metadata = {
  title: {
    default: "Ocellus",
    template: "%s | Ocellus",
  },
  description:
    "Ocellus is a hardware telemetry exporter for Intel server processors, with Prometheus metrics and Grafana dashboards.",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body suppressHydrationWarning>
        <header className="site-shell site-header">
          <nav className="nav" aria-label="Primary">
            <Link className="brand" href="/">
              Ocellus
            </Link>
            <div className="nav-links">
              <a href="https://github.com/minhuw/ocellus">GitHub</a>
            </div>
          </nav>
        </header>
        {children}
      </body>
    </html>
  );
}
