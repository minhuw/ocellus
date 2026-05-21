"use client";

import { useEffect, useId, useMemo, useRef, useState } from "react";
import type { DashboardEntry } from "../lib/dashboard-types";

type DashboardPickerProps = {
  dashboards: DashboardEntry[];
  version: string;
};

type ArchitectureAlias = {
  name: string;
  family: string;
  model: string;
  stepping?: string;
};

const architectureAliases: Record<string, ArchitectureAlias[]> = {
  "Sandy Bridge-EP / Ivy Bridge-EP": [
    {
      name: "Sandy Bridge-EP",
      family: "6",
      model: "0x2d",
    },
    {
      name: "Ivy Bridge-EP",
      family: "6",
      model: "0x3e",
    },
  ],
  "Haswell / Broadwell Xeon": [
    {
      name: "Haswell-EP",
      family: "6",
      model: "0x3f",
    },
    {
      name: "Broadwell-EP",
      family: "6",
      model: "0x4f",
    },
  ],
  "Skylake / Cascade Lake Xeon": [
    {
      name: "Skylake-SP",
      family: "6",
      model: "0x55",
    },
    {
      name: "Cascade Lake-SP",
      family: "6",
      model: "0x55",
    },
  ],
  "Ice Lake Xeon": [
    {
      name: "Ice Lake-SP",
      family: "6",
      model: "0x6a",
    },
  ],
  "Sapphire Rapids / Emerald Rapids Xeon": [
    {
      name: "Sapphire Rapids",
      family: "6",
      model: "0x8f",
    },
    {
      name: "Emerald Rapids",
      family: "6",
      model: "0xcf",
    },
  ],
};

type DashboardOption = {
  id: string;
  architecture: ArchitectureAlias;
  dashboard: DashboardEntry;
};

function dashboardOptions(dashboards: DashboardEntry[]): DashboardOption[] {
  return dashboards.flatMap((dashboard) => {
    const architectures = architectureAliases[dashboard.architecture] ?? [
      {
        name: dashboard.architecture,
        family: "varies",
        model: "varies",
      },
    ];
    return architectures.map((architecture) => ({
      id: `${dashboard.uid}:${architecture.name}`,
      architecture,
      dashboard,
    }));
  });
}

function optionText(option: DashboardOption): string {
  return `${option.architecture.name} family ${option.architecture.family} model ${option.architecture.model} ${option.architecture.stepping ?? ""} ${option.dashboard.title} ${option.dashboard.architecture} ${option.dashboard.file}`;
}

function dashboardUrl(dashboard: DashboardEntry, versionMode: string): string {
  if (versionMode === "latest") {
    return dashboard.url;
  }
  return dashboard.versionedUrl ?? dashboard.releaseUrl ?? dashboard.url;
}

export function DashboardPicker({ dashboards, version }: DashboardPickerProps) {
  const options = useMemo(() => dashboardOptions(dashboards), [dashboards]);
  const searchInputId = useId();
  const versionSelectId = useId();
  const listboxId = useId();
  const searchRef = useRef<HTMLInputElement>(null);
  const [isOpen, setIsOpen] = useState(false);
  const [copiedUrl, setCopiedUrl] = useState<string | null>(null);
  const [versionMode, setVersionMode] = useState("latest");
  const [selectedOptionId, setSelectedOptionId] = useState(options[0]?.id ?? "");
  const [query, setQuery] = useState(options[0]?.architecture.name ?? "");

  const selectedOption =
    options.find((option) => option.id === selectedOptionId) ?? options[0];

  const filteredOptions = useMemo(() => {
    const normalizedQuery = query.trim().toLowerCase();
    if (!normalizedQuery) {
      return options;
    }
    return options.filter((option) =>
      optionText(option).toLowerCase().includes(normalizedQuery),
    );
  }, [options, query]);

  const selectedDashboard = selectedOption?.dashboard;
  const selectedDashboardUrl = selectedDashboard
    ? dashboardUrl(selectedDashboard, versionMode)
    : "";
  const hasPinnedVersion = dashboards.some((dashboard) => dashboard.versionedUrl);

  useEffect(() => {
    if (selectedOption && !isOpen) {
      setQuery(selectedOption.architecture.name);
    }
  }, [isOpen, selectedOption]);

  function selectOption(option: DashboardOption) {
    setSelectedOptionId(option.id);
    setQuery(option.architecture.name);
    setIsOpen(false);
    searchRef.current?.focus();
  }

  function toggleModelMenu() {
    if (isOpen) {
      setIsOpen(false);
      setQuery(selectedOption?.architecture.name ?? "");
      return;
    }
    setQuery("");
    setIsOpen(true);
    searchRef.current?.focus();
  }

  async function copyDashboardUrl(url: string) {
    await navigator.clipboard.writeText(url);
    setCopiedUrl(url);
    window.setTimeout(() => setCopiedUrl(null), 1600);
  }

  return (
    <div className="dashboard-picker">
      <div className="dashboard-picker-controls">
        <div
          className="picker-field dashboard-combobox"
          onBlur={(event) => {
            if (!event.currentTarget.contains(event.relatedTarget)) {
              setIsOpen(false);
            }
          }}
        >
          <label htmlFor={searchInputId}>Select CPU model</label>
          <div className="combobox-control">
            <input
              id={searchInputId}
              ref={searchRef}
              type="search"
              role="combobox"
              aria-haspopup="listbox"
              aria-expanded={isOpen}
              aria-controls={listboxId}
              aria-autocomplete="list"
              value={query}
              onChange={(event) => {
                setQuery(event.target.value);
                setIsOpen(true);
              }}
              onFocus={() => setIsOpen(true)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  setIsOpen(false);
                  setQuery(selectedOption?.architecture.name ?? "");
                }
                if (event.key === "Enter" && filteredOptions[0]) {
                  event.preventDefault();
                  selectOption(filteredOptions[0]);
                }
              }}
              placeholder="Search by Xeon family, model, or dashboard"
            />
            <button
              type="button"
              className="combobox-toggle"
              aria-label={isOpen ? "Close CPU model list" : "Show CPU models"}
              aria-expanded={isOpen}
              aria-controls={listboxId}
              onMouseDown={(event) => event.preventDefault()}
              onClick={toggleModelMenu}
            >
              <span aria-hidden="true" />
            </button>
          </div>
          {isOpen ? (
            <div className="combobox-menu" id={listboxId} role="listbox">
              {filteredOptions.length > 0 ? (
                filteredOptions.map((option) => (
                  <button
                    key={option.id}
                    type="button"
                    className="combobox-option"
                    aria-selected={option.id === selectedOption?.id}
                    onMouseDown={(event) => event.preventDefault()}
                    onClick={() => selectOption(option)}
                    role="option"
                  >
                    <span>{option.architecture.name}</span>
                    <small>
                      family {option.architecture.family} · model{" "}
                      {option.architecture.model}
                    </small>
                  </button>
                ))
              ) : (
                <p className="combobox-empty">No dashboards match that search.</p>
              )}
            </div>
          ) : null}
        </div>

        <label
          className="picker-field dashboard-version-field"
          htmlFor={versionSelectId}
        >
          <span>Version</span>
          <div className="select-control">
            <select
              id={versionSelectId}
              value={versionMode}
              onChange={(event) => setVersionMode(event.target.value)}
            >
              <option value="latest">Latest</option>
              <option value={version} disabled={!hasPinnedVersion}>
                {version}
                {hasPinnedVersion ? "" : " (local build)"}
              </option>
            </select>
            <span aria-hidden="true" />
          </div>
        </label>
      </div>

      {selectedDashboard ? (
        <article className="selected-dashboard">
          <div className="dashboard-card-header">
            <div>
              <h3>{selectedOption.architecture.name}</h3>
              <p>
                Family {selectedOption.architecture.family} · Model{" "}
                {selectedOption.architecture.model}
                {selectedOption.architecture.stepping
                  ? ` · Stepping ${selectedOption.architecture.stepping}`
                  : ""}
              </p>
            </div>
            <span>Grafana dashboard</span>
          </div>

          <div className="dashboard-card-body">
            <p className="muted">Copy this URL into Grafana's dashboard import flow.</p>
          </div>

          <div className="dashboard-url-copy">
            <code
              className="dashboard-url-value"
              data-full-url={selectedDashboardUrl}
              tabIndex={0}
              title={selectedDashboardUrl}
            >
              {selectedDashboardUrl}
            </code>
            <button
              type="button"
              className="button secondary"
              onClick={() => void copyDashboardUrl(selectedDashboardUrl)}
            >
              {copiedUrl === selectedDashboardUrl ? "Copied" : "Copy URL"}
            </button>
          </div>
        </article>
      ) : (
        <p className="muted">No dashboards match that search.</p>
      )}
    </div>
  );
}
