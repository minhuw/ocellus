type JsonObject = Record<string, unknown>;

type GrafanaDataQueryV2 = {
  kind: "DataQuery";
  group: string;
  version?: string;
  datasource?: {
    name?: string;
    uid?: string;
    type?: string;
  };
  spec: JsonObject;
};

type GrafanaPanelQueryV2 = {
  kind: "PanelQuery";
  spec: {
    query: GrafanaDataQueryV2;
    refId?: string;
    hidden?: boolean;
  };
};

type GrafanaVariableV2 = {
  kind: string;
  spec: JsonObject & {
    query?: string | GrafanaDataQueryV2;
    refresh?: string;
    hide?: string;
    sort?: string;
  };
};

type GrafanaPanelV2 = {
  kind: "Panel";
  spec: {
    id?: number;
    title?: string;
    description?: string;
    links?: unknown[];
    data?: {
      kind: "QueryGroup";
      spec: {
        queries?: GrafanaPanelQueryV2[];
        transformations?: unknown[];
        queryOptions?: JsonObject;
      };
    };
    vizConfig?: {
      kind: "VizConfig";
      group: string;
      version?: string;
      spec: {
        fieldConfig?: JsonObject;
        options?: JsonObject;
      };
    };
    transparent?: boolean;
  };
};

type GrafanaElementV2 = GrafanaPanelV2;

type GrafanaGridLayoutItemV2 = {
  kind: "GridLayoutItem";
  spec: {
    x: number;
    y: number;
    width: number;
    height: number;
    element: {
      kind: "ElementReference";
      name: string;
    };
  };
};

type GrafanaGridLayoutV2 = {
  kind: "GridLayout";
  spec: {
    items: GrafanaGridLayoutItemV2[];
  };
};

type GrafanaRowsLayoutRowV2 = {
  kind: "RowsLayoutRow";
  spec: {
    title: string;
    collapse?: boolean;
    layout: GrafanaGridLayoutV2;
  };
};

type GrafanaRowsLayoutV2 = {
  kind: "RowsLayout";
  spec: {
    rows: GrafanaRowsLayoutRowV2[];
  };
};

type GrafanaTabsLayoutTabV2 = {
  kind: "TabsLayoutTab";
  spec: {
    title: string;
    layout: GrafanaGridLayoutV2 | GrafanaRowsLayoutV2;
    variables?: GrafanaVariableV2[];
  };
};

type GrafanaTabsLayoutV2 = {
  kind: "TabsLayout";
  spec: {
    tabs: GrafanaTabsLayoutTabV2[];
  };
};

export type GrafanaDashboardV2 = {
  apiVersion: "dashboard.grafana.app/v2";
  kind: "Dashboard";
  metadata?: {
    name?: string;
  };
  spec: {
    title: string;
    description?: string;
    tags?: string[];
    editable?: boolean;
    links?: unknown[];
    liveNow?: boolean;
    annotations?: unknown[];
    variables?: GrafanaVariableV2[];
    timeSettings?: {
      timezone?: string;
      from?: string;
      to?: string;
      autoRefresh?: string;
      autoRefreshIntervals?: string[];
      hideTimepicker?: boolean;
      fiscalYearStartMonth?: number;
    };
    elements: Record<string, GrafanaElementV2>;
    layout: GrafanaTabsLayoutV2 | GrafanaGridLayoutV2 | GrafanaRowsLayoutV2;
  };
};

type GrafanaClassicPanel = JsonObject & {
  id: number;
  type: string;
  title: string;
  gridPos: {
    x: number;
    y: number;
    w: number;
    h: number;
  };
};

type GrafanaClassicDashboard = JsonObject & {
  __inputs: unknown[];
  __requires: unknown[];
  annotations: {
    list: unknown[];
  };
  editable: boolean;
  graphTooltip: number;
  links: unknown[];
  liveNow: boolean;
  panels: GrafanaClassicPanel[];
  refresh: string;
  schemaVersion: number;
  style: string;
  tags: string[];
  templating: {
    list: JsonObject[];
  };
  time: {
    from: string;
    to: string;
  };
  timepicker: {
    refresh_intervals: string[];
  };
  timezone: string;
  title: string;
  uid: string | null;
  version: number;
};

const classicSchemaVersion = 39;
const prometheusInputName = "DS_PROMETHEUS";
const gridWidth = 24;
const rowHeight = 1;

function datasourceRef(): JsonObject {
  return {
    type: "prometheus",
    uid: `\${${prometheusInputName}}`,
  };
}

function convertRefresh(refresh?: string): number | undefined {
  switch (refresh) {
    case "never":
      return 0;
    case "onDashboardLoad":
      return 1;
    case "onTimeRangeChanged":
      return 2;
    default:
      return undefined;
  }
}

function convertHide(hide?: string): number {
  switch (hide) {
    case "hideLabel":
      return 1;
    case "hideVariable":
      return 2;
    default:
      return 0;
  }
}

function convertSort(sort?: string): number {
  switch (sort) {
    case "alphabeticalAsc":
      return 1;
    case "alphabeticalDesc":
      return 2;
    case "numericalAsc":
      return 3;
    case "numericalDesc":
      return 4;
    case "alphabeticalCaseInsensitiveAsc":
      return 5;
    case "alphabeticalCaseInsensitiveDesc":
      return 6;
    default:
      return 0;
  }
}

function convertVariable(variable: GrafanaVariableV2): JsonObject {
  const { query, refresh, hide, sort, ...rest } = variable.spec;
  const classic: JsonObject = {
    ...rest,
    hide: convertHide(hide),
    type: variable.kind === "QueryVariable" ? "query" : "custom",
  };

  if (refresh !== undefined) {
    classic.refresh = convertRefresh(refresh);
  }
  if (sort !== undefined) {
    classic.sort = convertSort(sort);
  }

  if (typeof query === "object" && query?.kind === "DataQuery") {
    classic.datasource = datasourceRef();
    classic.query = query.spec.query ?? "";
    classic.definition = query.spec.query ?? classic.definition ?? "";
  } else {
    classic.query = query ?? "";
  }

  return classic;
}

function convertTarget(query: GrafanaPanelQueryV2): JsonObject {
  const dataQuery = query.spec.query;
  if (dataQuery.kind !== "DataQuery") {
    throw new Error(`unsupported query kind: ${dataQuery.kind}`);
  }

  return {
    ...dataQuery.spec,
    datasource: datasourceRef(),
    refId: query.spec.refId,
    hide: query.spec.hidden,
  };
}

function convertPanel(
  id: number,
  elementName: string,
  item: GrafanaGridLayoutItemV2,
  elements: Record<string, GrafanaElementV2>,
  yOffset: number,
): GrafanaClassicPanel {
  const element = elements[elementName];
  if (!element) {
    throw new Error(`layout references missing element: ${elementName}`);
  }
  if (element.kind !== "Panel") {
    throw new Error(`unsupported element kind for ${elementName}: ${element.kind}`);
  }

  const panel = element.spec;
  const queryGroup = panel.data?.spec;
  const vizConfig = panel.vizConfig;
  if (!vizConfig) {
    throw new Error(`panel ${elementName} is missing vizConfig`);
  }
  if (vizConfig.group !== "timeseries") {
    throw new Error(`unsupported panel visualization for ${elementName}: ${vizConfig.group}`);
  }

  return {
    datasource: queryGroup?.queries?.[0]
      ? datasourceRef()
      : undefined,
    description: panel.description ?? "",
    fieldConfig: vizConfig.spec.fieldConfig ?? {},
    gridPos: {
      h: item.spec.height,
      w: item.spec.width,
      x: item.spec.x,
      y: yOffset + item.spec.y,
    },
    id,
    links: panel.links ?? [],
    options: vizConfig.spec.options ?? {},
    targets: queryGroup?.queries?.map(convertTarget) ?? [],
    title: panel.title ?? elementName,
    transformations: queryGroup?.transformations ?? [],
    transparent: panel.transparent ?? false,
    type: "timeseries",
  };
}

function gridLayoutHeight(layout: GrafanaGridLayoutV2): number {
  if (layout.spec.items.length === 0) {
    return 0;
  }

  return Math.max(
    ...layout.spec.items.map((item) => item.spec.y + item.spec.height),
  );
}

function convertGridLayout(
  layout: GrafanaGridLayoutV2,
  elements: Record<string, GrafanaElementV2>,
  yOffset: number,
  nextPanelId: () => number,
): GrafanaClassicPanel[] {
  return layout.spec.items.map((item) => {
    const reference = item.spec.element;
    if (reference.kind !== "ElementReference") {
      throw new Error(`unsupported grid item element kind: ${reference.kind}`);
    }
    return convertPanel(nextPanelId(), reference.name, item, elements, yOffset);
  });
}

function rowPanel(
  id: number,
  title: string,
  y: number,
  collapsed: boolean,
): GrafanaClassicPanel {
  return {
    collapsed,
    datasource: null,
    gridPos: {
      h: rowHeight,
      w: gridWidth,
      x: 0,
      y,
    },
    id,
    panels: [],
    title,
    type: "row",
  };
}

function convertRowsLayout(
  layout: GrafanaRowsLayoutV2,
  elements: Record<string, GrafanaElementV2>,
  yOffset: number,
  nextPanelId: () => number,
): GrafanaClassicPanel[] {
  const panels: GrafanaClassicPanel[] = [];
  let currentY = yOffset;

  for (const row of layout.spec.rows) {
    const collapsed = row.spec.collapse ?? false;
    panels.push(rowPanel(nextPanelId(), row.spec.title, currentY, collapsed));

    const rowPanels = convertGridLayout(
      row.spec.layout,
      elements,
      currentY + rowHeight,
      nextPanelId,
    );
    panels.push(...rowPanels);
    currentY += rowHeight + gridLayoutHeight(row.spec.layout);
  }

  return panels;
}

function convertLayout(
  layout: GrafanaDashboardV2["spec"]["layout"],
  elements: Record<string, GrafanaElementV2>,
): GrafanaClassicPanel[] {
  let panelId = 1;
  const nextPanelId = () => panelId++;

  if (layout.kind === "GridLayout") {
    return convertGridLayout(layout, elements, 0, nextPanelId);
  }

  if (layout.kind === "RowsLayout") {
    return convertRowsLayout(layout, elements, 0, nextPanelId);
  }

  const panels: GrafanaClassicPanel[] = [];
  let currentY = 0;

  for (const tab of layout.spec.tabs) {
    panels.push(rowPanel(nextPanelId(), tab.spec.title, currentY, false));

    if (tab.spec.layout.kind === "GridLayout") {
      const tabPanels = convertGridLayout(
        tab.spec.layout,
        elements,
        currentY + rowHeight,
        nextPanelId,
      );
      panels.push(...tabPanels);
      currentY += rowHeight + gridLayoutHeight(tab.spec.layout);
    } else if (tab.spec.layout.kind === "RowsLayout") {
      const tabPanels = convertRowsLayout(
        tab.spec.layout,
        elements,
        currentY + rowHeight,
        nextPanelId,
      );
      panels.push(...tabPanels);
      const height = tabPanels.reduce(
        (maxY, panel) => Math.max(maxY, panel.gridPos.y + panel.gridPos.h),
        currentY + rowHeight,
      );
      currentY = height;
    }
  }

  return panels;
}

function variableName(variable: GrafanaVariableV2): string {
  const name = variable.spec.name;
  if (typeof name !== "string" || name.length === 0) {
    throw new Error("dashboard variable is missing a name");
  }
  return name;
}

function appendVariable(
  variables: Map<string, GrafanaVariableV2>,
  variable: GrafanaVariableV2,
): void {
  const name = variableName(variable);
  const existing = variables.get(name);
  if (existing && JSON.stringify(existing) !== JSON.stringify(variable)) {
    throw new Error(`dashboard variable ${name} is defined more than once`);
  }
  variables.set(name, variable);
}

function collectLayoutVariables(
  layout: GrafanaDashboardV2["spec"]["layout"],
  variables: Map<string, GrafanaVariableV2>,
): void {
  if (layout.kind !== "TabsLayout") {
    return;
  }

  for (const tab of layout.spec.tabs) {
    for (const variable of tab.spec.variables ?? []) {
      appendVariable(variables, variable);
    }
    collectLayoutVariables(tab.spec.layout, variables);
  }
}

function collectVariables(dashboard: GrafanaDashboardV2): GrafanaVariableV2[] {
  const variables = new Map<string, GrafanaVariableV2>();

  for (const variable of dashboard.spec.variables ?? []) {
    appendVariable(variables, variable);
  }
  collectLayoutVariables(dashboard.spec.layout, variables);

  return [...variables.values()];
}

export function classicDashboardFile(file: string): string {
  return file.replace(/\.json$/u, ".classic.json");
}

export function convertDashboardV2ToClassic(
  dashboard: GrafanaDashboardV2,
): GrafanaClassicDashboard {
  if (dashboard.apiVersion !== "dashboard.grafana.app/v2") {
    throw new Error(`unsupported dashboard apiVersion: ${dashboard.apiVersion}`);
  }
  if (dashboard.kind !== "Dashboard") {
    throw new Error(`unsupported resource kind: ${dashboard.kind}`);
  }

  const timeSettings = dashboard.spec.timeSettings ?? {};
  const variables = collectVariables(dashboard);

  return {
    __inputs: [
      {
        name: prometheusInputName,
        label: "Prometheus",
        description: "",
        type: "datasource",
        pluginId: "prometheus",
        pluginName: "Prometheus",
      },
    ],
    __requires: [
      {
        type: "grafana",
        id: "grafana",
        name: "Grafana",
        version: "10.0.0",
      },
      {
        type: "datasource",
        id: "prometheus",
        name: "Prometheus",
        version: "1.0.0",
      },
      {
        type: "panel",
        id: "timeseries",
        name: "Time series",
        version: "",
      },
    ],
    annotations: {
      list: dashboard.spec.annotations ?? [],
    },
    description: dashboard.spec.description ?? "",
    editable: dashboard.spec.editable ?? true,
    fiscalYearStartMonth: timeSettings.fiscalYearStartMonth ?? 0,
    graphTooltip: 0,
    links: dashboard.spec.links ?? [],
    liveNow: dashboard.spec.liveNow ?? false,
    panels: convertLayout(dashboard.spec.layout, dashboard.spec.elements),
    refresh: timeSettings.autoRefresh ?? "",
    schemaVersion: classicSchemaVersion,
    style: "dark",
    tags: dashboard.spec.tags ?? [],
    templating: {
      list: variables.map(convertVariable),
    },
    time: {
      from: timeSettings.from ?? "now-15m",
      to: timeSettings.to ?? "now",
    },
    timepicker: {
      refresh_intervals: timeSettings.autoRefreshIntervals ?? [],
    },
    timezone: timeSettings.timezone ?? "browser",
    title: dashboard.spec.title,
    uid: dashboard.metadata?.name ?? null,
    version: 1,
    weekStart: "",
  };
}
