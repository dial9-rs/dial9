import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const indexHtml = readFileSync(
  fileURLToPath(new URL("../index.html", import.meta.url)),
  "utf8",
);

function inlineFunction(name: string, followingComment: string): string {
  const pattern = new RegExp(
    `    async function ${name}\\(\\) \\{([\\s\\S]*?)\\n    \\}\\n\\n    // ${followingComment}`,
  );
  const match = indexHtml.match(pattern);
  expect(match, `canonical index.html must define ${name}()`).not.toBeNull();
  return `async function ${name}() {${match![1]!}\n    }`;
}

describe("canonical browser service deep links", () => {
  it("bypasses service discovery and opens exactly the requested service", async () => {
    const discoverServicesSource = inlineFunction(
      "discoverServices",
      "Re-discover when bucket changes",
    );
    const run = new Function(`
      const bucketInput = { value: "example-traces-bucket" };
      const prefixInput = { value: "" };
      const serviceInput = { value: "example-service" };
      const browseStatus = { textContent: "", className: "status", style: { display: "" } };
      const fields = {
        "search-btn": { disabled: false },
        "range-from": { value: "2026-07-24T18:49" },
        "range-to": { value: "2026-07-24T19:49" },
      };
      const document = { getElementById: (id) => fields[id] };
      const pickerToDate = (value) => new Date(value + ":00Z");
      let serverHasPrefix = false;
      let serviceDiscoveryGeneration = 0;
      let browseGeneration = 0;
      let availableServices = [];
      let availableServiceMetadata = new Map();
      let discoveryRequests = 0;
      let browseRequests = 0;
      const syncedServices = [];
      const resetBrowsePane = () => {
        availableServices = [];
        availableServiceMetadata = new Map();
        serviceInput.value = "";
      };
      const renderServiceTabs = () => {};
      const setBrowseWarning = () => {};
      const syncUrl = () => syncedServices.push(serviceInput.value);
      const doTimeRangeSearch = async () => { browseRequests += 1; };
      const apiFetch = async () => {
        discoveryRequests += 1;
        return {
          ok: true,
          json: async () => ({ services: [], service_metadata: [] }),
          text: async () => "",
        };
      };
      ${discoverServicesSource}
      return discoverServices().then(() => ({
        discoveryRequests,
        browseRequests,
        availableServices,
        service: serviceInput.value,
        syncedServices,
        status: browseStatus.textContent,
      }));
    `);

    const result = (await run()) as {
      discoveryRequests: number;
      browseRequests: number;
      availableServices: string[];
      service: string;
      syncedServices: string[];
      status: string;
    };

    expect(result.discoveryRequests).toBe(0);
    expect(result.browseRequests).toBe(1);
    expect(result.availableServices).toEqual(["example-service"]);
    expect(result.service).toBe("example-service");
    expect(result.syncedServices).toContain("example-service");
    expect(result.status).toBe("Loading service…");
  });
});
