// mimic UI (spec 7.5). Fills the static pages from the JSON API. Every recorded value is
// written with textContent or setAttribute on an encoded path: no markup is ever built from it.
(function () {
  "use strict";

  var app = document.getElementById("app");

  function el(tag, text, attrs) {
    var node = document.createElement(tag);
    if (text !== undefined && text !== null) {
      node.textContent = String(text);
    }
    if (attrs) {
      Object.keys(attrs).forEach(function (name) {
        node.setAttribute(name, attrs[name]);
      });
    }
    return node;
  }

  function add(parent, children) {
    children.forEach(function (child) {
      parent.appendChild(child);
    });
    return parent;
  }

  function show(nodes) {
    while (app.firstChild) {
      app.removeChild(app.firstChild);
    }
    add(app, nodes);
  }

  function enc(part) {
    return encodeURIComponent(part);
  }

  function api(path) {
    return fetch(path, { credentials: "omit", headers: { Accept: "application/json" } }).then(
      function (response) {
        if (response.status === 404) {
          return null;
        }
        if (!response.ok) {
          throw new Error("HTTP " + response.status);
        }
        return response.json();
      }
    );
  }

  function failed(error) {
    show([el("p", "Error: " + error.message, { class: "error" })]);
  }

  // A cell is a node or a value written as text.
  function table(head, rows) {
    var headRow = add(el("tr"), head.map(function (name) {
      return el("th", name);
    }));
    var body = add(el("tbody"), rows.map(function (row) {
      return add(el("tr"), row.map(function (cell) {
        if (cell !== null && typeof cell === "object") {
          return add(el("td"), [cell]);
        }
        return el("td", cell === undefined || cell === null ? "" : cell);
      }));
    }));
    return add(el("table"), [add(el("thead"), [headRow]), body]);
  }

  function link(text, href) {
    return el("a", text, { href: href });
  }

  function workloadPath(ns, dep) {
    return "/ui/workloads/" + enc(ns) + "/" + enc(dep);
  }

  function apiPath(ns, dep, tail) {
    return "/v1/workloads/" + enc(ns) + "/" + enc(dep) + (tail || "");
  }

  function pairs(object) {
    return Object.keys(object || {}).sort().map(function (key) {
      return key + ": " + object[key];
    }).join(", ");
  }

  function resourcePath(ns, dep, kind, name) {
    return workloadPath(ns, dep) + "/resources/" + enc(kind) + "/" + enc(name);
  }

  // Marks the tab of the current page; the tabs themselves are static HTML.
  function markTab(path) {
    var tabs = document.querySelectorAll("#tabs a");
    Array.prototype.forEach.call(tabs, function (tab) {
      if (tab.getAttribute("href") === path) {
        tab.setAttribute("aria-current", "page");
      }
    });
  }

  // The Map tab: the SVG the controller rendered is parsed and copied element by element, from
  // an allowlist of elements and attributes, with links only under /ui/. Nothing of it is ever
  // handed to the page as markup.
  var SVG_ELEMENTS = ["svg", "defs", "marker", "path", "g", "a", "rect", "text"];
  var SVG_ATTRIBUTES = ["class", "x", "y", "width", "height", "viewBox", "d", "fill", "stroke",
    "stroke-width", "stroke-dasharray", "marker-end", "id", "markerWidth", "markerHeight", "refX",
    "refY", "orient", "href", "role", "aria-label", "text-anchor", "font-size", "font-family", "rx"];

  // Written in two parts: no URL appears in this file (UI-09).
  var SVG_NAMESPACE = "http:" + "//www.w3.org/2000/svg";

  // A link stays inside the UI: under /ui/, with no empty, "." or ".." segment (written plain or
  // percent-encoded), no backslash and no control character (a browser strips or rewrites them).
  function safeHref(value) {
    if (value.indexOf("/ui/") !== 0 || value.indexOf("\\") >= 0 || /[\u0000-\u001f\u007f]/.test(value)) {
      return false;
    }
    var path = value.split(/[?#]/)[0];
    if (path.indexOf("//") >= 0) {
      return false;
    }
    return path.split("/").every(function (segment) {
      var decoded;
      try {
        decoded = decodeURIComponent(segment);
      } catch (e) {
        return false;
      }
      return decoded !== "." && decoded !== ".." && decoded.indexOf("/") < 0 &&
        decoded.indexOf("\\") < 0;
    });
  }

  function copySvg(source, ns) {
    var node = document.createElementNS(ns, source.localName);
    Array.prototype.forEach.call(source.attributes, function (attr) {
      if (SVG_ATTRIBUTES.indexOf(attr.name) < 0) {
        return;
      }
      if (attr.name === "href" && !safeHref(attr.value)) {
        return;
      }
      node.setAttribute(attr.name, attr.value);
    });
    Array.prototype.forEach.call(source.childNodes, function (child) {
      if (child.nodeType === 3) {
        node.appendChild(document.createTextNode(child.nodeValue));
      } else if (child.nodeType === 1 && child.namespaceURI === ns &&
          SVG_ELEMENTS.indexOf(child.localName) >= 0) {
        node.appendChild(copySvg(child, ns));
      }
    });
    return node;
  }

  function mapPage() {
    fetch("/v1/map?format=svg", { credentials: "omit", headers: { Accept: "image/svg+xml" } }).then(
      function (response) {
        if (!response.ok) {
          throw new Error("HTTP " + response.status);
        }
        return response.text();
      }
    ).then(function (text) {
      var doc = new DOMParser().parseFromString(text, "image/svg+xml");
      var root = doc.documentElement;
      if (!root || root.localName !== "svg" ||
          root.namespaceURI !== SVG_NAMESPACE || doc.getElementsByTagName("parsererror").length > 0) {
        throw new Error("the map is not readable");
      }
      if (root.getElementsByTagName("rect").length === 0) {
        show([el("h1", "Map"), el("p", "No calls recorded")]);
        return;
      }
      var holder = el("div", null, { class: "map" });
      holder.appendChild(copySvg(root, root.namespaceURI));
      show([
        el("h1", "Map"),
        el("p", "Deployments, Services and external hosts, with the calls the sidecars recorded " +
          "between them: totals, and the share of 5xx answers. A dashed red arrow is a call to a " +
          "Service of another namespace that is not declared."),
        holder
      ]);
    }).catch(failed);
  }

  // The Workloads tab: every deployment with data in the store or named by a mimic resource.
  function listPage() {
    Promise.all([api("/v1/workloads"), api("/v1/resources")]).then(function (results) {
      var rows = {};
      function row(ns, dep) {
        var id = ns + "/" + dep;
        if (!rows[id]) {
          rows[id] = { ns: ns, dep: dep, resources: [] };
        }
        return rows[id];
      }
      ((results[0] && results[0].workloads) || []).forEach(function (w) {
        row(w.namespace, w.deployment).stored = w;
      });
      ((results[1] && results[1].workloads) || []).forEach(function (w) {
        var r = row(w.namespace, w.deployment);
        r.mode = w.mode;
        r.resources = w.resources;
      });
      var ids = Object.keys(rows).sort();
      if (ids.length === 0) {
        show([el("h1", "Workloads"), el("p", "No data")]);
        return;
      }
      return Promise.all(ids.map(function (id) {
        var r = rows[id];
        return r.stored ? api(apiPath(r.ns, r.dep)) : Promise.resolve(null);
      })).then(function (details) {
        show([el("h1", "Workloads"), table(
          ["Namespace", "Deployment", "Mode", "Pods", "Endpoints", "Exchanges", "Last push",
            "Resources"],
          ids.map(function (id, i) {
            var r = rows[id];
            var stored = r.stored || {};
            var pods = details[i] ? Object.keys(details[i].pods || {}).length : "";
            var links = add(el("ul", null, { class: "plain" }), r.resources.map(function (res) {
              return add(el("li"), [link(res.kind + " " + res.name,
                resourcePath(r.ns, r.dep, res.kind, res.name))]);
            }));
            return [r.ns, link(r.dep, workloadPath(r.ns, r.dep)), r.mode || "", pods,
              stored.endpoints, stored.exchanges, stored.last_push, links];
          })
        )]);
      });
    }).catch(failed);
  }

  // The resource view: the spec as read-only YAML, labels, annotations and status conditions.
  function resourcePage(ns, dep, kind, name) {
    api(apiPath(ns, dep, "/resources")).then(function (data) {
      var found = ((data && data.resources) || []).filter(function (r) {
        return r.kind === kind && r.name === name;
      })[0];
      var title = el("h1", kind + " " + name);
      var back = el("p", null);
      add(back, [link("Back to " + ns + "/" + dep, workloadPath(ns, dep))]);
      if (!found) {
        show([title, back, el("p", "No such resource")]);
        return;
      }
      var meta = found.metadata || {};
      show([
        title,
        back,
        table(["Namespace", "Created", "Generation"],
          [[found.namespace, meta.creationTimestamp, meta.generation]]),
        el("h2", "Labels"),
        table(["Name", "Value"], Object.keys(meta.labels || {}).sort().map(function (k) {
          return [k, meta.labels[k]];
        })),
        el("h2", "Annotations"),
        table(["Name", "Value"], Object.keys(meta.annotations || {}).sort().map(function (k) {
          return [k, meta.annotations[k]];
        })),
        el("h2", "Spec"),
        el("pre", found.yaml),
        el("h2", "Status"),
        found.phase ? el("p", "Phase: " + found.phase) : el("p", null),
        table(["Type", "Status", "Reason", "Message", "Last transition"],
          (found.conditions || []).map(function (c) {
            return [c.type, c.status, c.reason, c.message, c.lastTransitionTime];
          }))
      ]);
    }).catch(failed);
  }

  function workloadPage(ns, dep) {
    Promise.all([
      api(apiPath(ns, dep)),
      api(apiPath(ns, dep, "/endpoints")),
      api(apiPath(ns, dep, "/openapi")),
      api(apiPath(ns, dep, "/loadtests"))
    ]).then(function (results) {
      var summary = results[0];
      var title = el("h1", ns + "/" + dep);
      if (summary === null) {
        show([title, el("p", "No data")]);
        return;
      }
      var pods = Object.keys(summary.pods || {}).sort();
      var documents = (results[2] && results[2].documents) || [];
      var endpoints = (results[1] && results[1].endpoints) || [];
      var docs = add(el("ul"), documents.map(function (doc) {
        var tunnels = doc.tunnels ? ", " + doc.tunnels + " tunnels" : "";
        return add(el("li"), [
          link(doc.id, workloadPath(ns, dep) + "/openapi/" + enc(doc.id)),
          document.createTextNode(" (" + doc.endpoints + " endpoints" + tunnels + ")")
        ]);
      }));
      var runs = (results[3] && results[3].loadtests) || [];
      show([
        title,
        table(["Endpoints", "Exchanges", "Sample bytes", "Last push"],
          [[summary.endpoints, summary.exchanges, summary.sample_bytes, summary.last_push]]),
        el("h2", "Pods"),
        table(["Pod", "Last push"], pods.map(function (pod) {
          return [pod, summary.pods[pod]];
        })),
        el("h2", "OpenAPI documents"),
        docs,
        el("h2", "Endpoints"),
        table(
          ["Direction", "Port", "Authority", "Method", "Template", "Exchanges", "Statuses",
            "p50 ms", "p90 ms", "p99 ms", "Last seen"],
          endpoints.map(function (e) {
            var latency = e.latency_ms || {};
            var authority = e.authority === undefined ? "" : link(
              e.authority, workloadPath(ns, dep) + "/openapi/" + enc(e.document));
            return [e.direction, e.port, authority, e.method, e.template, e.exchanges,
              pairs(e.statuses), latency.p50, latency.p90, latency.p99, e.last_seen];
          })
        ),
        el("h2", "Load tests"),
        table(
          ["Name", "Phase", "Reason", "Start", "End", "Requests", "Missed", "Statuses", "Errors",
            "RPS", "p99 \u00b5s"],
          runs.map(function (r) {
            var total = r.total || {};
            var latency = total.latencyMicros || {};
            return [r.name, r.phase, r.reason, r.start, r.end, total.requests, total.missed,
              pairs(total.statuses), pairs(total.errors), total.rps, latency.p99];
          })
        )
      ]);
    }).catch(failed);
  }

  // Swagger UI falls back to the original request when an interceptor returns a falsy value, so
  // a refusal must throw: the request is then never sent.
  function requestInterceptor(request) {
    var target;
    try {
      target = new URL(request.url, window.location.href);
    } catch (error) {
      throw new Error("request refused: malformed URL");
    }
    if (target.origin !== window.location.origin ||
        target.pathname.indexOf("/v1/workloads/") !== 0) {
      throw new Error("request refused: only same-origin /v1/workloads/ routes are allowed");
    }
    return request;
  }
  window.mimicRequestInterceptor = requestInterceptor;

  function openapiPage(ns, dep, id) {
    var url = apiPath(ns, dep, "/openapi/" + enc(id));
    show([
      el("h1", ns + "/" + dep + " " + id),
      add(el("p"), [
        link("Back to " + ns + "/" + dep, workloadPath(ns, dep)),
        document.createTextNode(" | "),
        link("Raw JSON", url)
      ])
    ]);
    if (typeof window.SwaggerUIBundle !== "function") {
      return;
    }
    window.SwaggerUIBundle({
      url: url,
      dom_id: "#swagger-ui",
      layout: "BaseLayout",
      supportedSubmitMethods: [],
      tryItOutEnabled: false,
      validatorUrl: null,
      queryConfigEnabled: false,
      withCredentials: false,
      showExtensions: true,
      deepLinking: true,
      requestInterceptor: requestInterceptor
    });
  }

  // The Endpoints tab: the deployment picker and four filters, applied in the browser to the
  // rows of the picked deployment's endpoint list. Nothing is requested when a filter changes.
  function statusClasses(statuses) {
    return Object.keys(statuses || {}).map(function (code) {
      return code.charAt(0) + "xx";
    });
  }

  function endpointsPage() {
    var wanted = new URLSearchParams(window.location.search).get("deployment") || "";
    api("/v1/workloads").then(function (data) {
      var ids = ((data && data.workloads) || []).map(function (w) {
        return w.namespace + "/" + w.deployment;
      }).sort();
      var picker = el("select", null, { id: "filter-deployment" });
      add(picker, [el("option", "Pick a deployment", { value: "" })].concat(ids.map(function (id) {
        return el("option", id, { value: id });
      })));
      picker.value = ids.indexOf(wanted) >= 0 ? wanted : "";
      var method = el("select", null, { id: "filter-method" });
      var template = el("input", null, { id: "filter-template", type: "text" });
      var authority = el("input", null, { id: "filter-authority", type: "text" });
      var status = el("select", null, { id: "filter-status" });
      add(status, ["", "2xx", "3xx", "4xx", "5xx"].map(function (c) {
        return el("option", c === "" ? "Any status" : c, { value: c });
      }));
      var result = el("div");
      var rows = [];
      var ns = "";
      var dep = "";

      function label(text, control) {
        return add(el("label"), [document.createTextNode(text + " "), control]);
      }

      function render() {
        var shown = rows.filter(function (e) {
          return (method.value === "" || e.method === method.value) &&
            (template.value === "" ||
              String(e.template).toLowerCase().indexOf(template.value.toLowerCase()) >= 0) &&
            (authority.value === "" ||
              String(e.authority || "").toLowerCase().indexOf(authority.value.toLowerCase()) >= 0) &&
            (status.value === "" || statusClasses(e.statuses).indexOf(status.value) >= 0);
        });
        if (shown.length === 0) {
          fill(result, [el("p", "No endpoint matches")]);
          return;
        }
        fill(result, [table(
          ["Direction", "Port", "Authority", "Method", "Template", "Exchanges", "Statuses",
            "p50 ms", "p90 ms", "p99 ms", "Last seen", "Document"],
          shown.map(function (e) {
            var latency = e.latency_ms || {};
            return [e.direction, e.port, e.authority, e.method, e.template, e.exchanges,
              pairs(e.statuses), latency.p50, latency.p90, latency.p99, e.last_seen,
              link(e.document, workloadPath(ns, dep) + "/openapi/" + enc(e.document))];
          })
        )]);
      }

      function fill(parent, nodes) {
        while (parent.firstChild) {
          parent.removeChild(parent.firstChild);
        }
        add(parent, nodes);
      }

      function load() {
        var id = picker.value;
        var slash = id.indexOf("/");
        fill(result, []);
        if (id === "") {
          return Promise.resolve();
        }
        ns = id.slice(0, slash);
        dep = id.slice(slash + 1);
        return api(apiPath(ns, dep, "/endpoints")).then(function (list) {
          if (list === null) {
            fill(result, [el("p", "No data")]);
            return;
          }
          rows = list.endpoints || [];
          var methods = {};
          rows.forEach(function (e) {
            methods[e.method] = true;
          });
          fill(method, [el("option", "Any method", { value: "" })].concat(
            Object.keys(methods).sort().map(function (m) {
              return el("option", m, { value: m });
            })));
          render();
        });
      }

      picker.addEventListener("change", function () {
        var url = window.location.pathname +
          (picker.value === "" ? "" : "?deployment=" + enc(picker.value).replace(/%2F/g, "/"));
        window.history.replaceState(null, "", url);
        load().catch(failed);
      });
      [method, status].forEach(function (control) {
        control.addEventListener("change", render);
      });
      [template, authority].forEach(function (control) {
        control.addEventListener("input", render);
      });
      show([
        el("h1", "Endpoints"),
        add(el("p", null, { class: "filters" }), [
          label("Deployment", picker), label("Method", method), label("Template", template),
          label("Authority", authority), label("Status", status)
        ]),
        result
      ]);
      if (ids.length === 0) {
        fill(result, [el("p", "No data")]);
        return;
      }
      return load();
    }).catch(failed);
  }

  // The Load tests tab: the runs of every deployment, from the stored summaries. Hidden, with
  // its reason, when the controller runs without load tests.
  function runPath(ns, dep, uid) {
    return workloadPath(ns, dep) + "/loadtests/" + enc(uid);
  }

  function loadTestsPage() {
    api("/v1/features").then(function (features) {
      var enabled = features && features.loadTest && features.loadTest.enabled === true;
      if (!enabled) {
        show([
          el("h1", "Load tests"),
          el("p", "Load testing is disabled in this install: set loadTest.enabled=true in the " +
            "chart to run MimicLoadTest objects and list their runs here.")
        ]);
        return;
      }
      return Promise.all([api("/v1/workloads"), api("/v1/resources")]).then(function (results) {
        var found = {};
        ((results[0] && results[0].workloads) || []).concat(
          (results[1] && results[1].workloads) || []).forEach(function (w) {
          found[w.namespace + "/" + w.deployment] = [w.namespace, w.deployment];
        });
        var ids = Object.keys(found).sort();
        return Promise.all(ids.map(function (id) {
          return api(apiPath(found[id][0], found[id][1], "/loadtests"));
        })).then(function (lists) {
          var rows = [];
          ids.forEach(function (id, i) {
            var ns = found[id][0];
            var dep = found[id][1];
            ((lists[i] && lists[i].loadtests) || []).forEach(function (r) {
              var total = r.total || {};
              var latency = total.latencyMicros || {};
              rows.push([link(id, workloadPath(ns, dep)), link(r.name || r.uid,
                runPath(ns, dep, r.uid)), r.phase, r.reason, total.requests, total.rps,
                latency.p99, r.start, r.end]);
            });
          });
          if (rows.length === 0) {
            show([el("h1", "Load tests"), el("p", "No load test runs")]);
            return;
          }
          show([el("h1", "Load tests"), table(
            ["Deployment", "Name", "Phase", "Reason", "Requests", "RPS", "p99 µs", "Start",
              "End"], rows)]);
        });
      });
    }).catch(failed);
  }

  // The run page: the total, the error kinds and one row per endpoint of the stored summary.
  function runPage(ns, dep, uid) {
    api(apiPath(ns, dep, "/loadtests/" + enc(uid))).then(function (run) {
      var back = add(el("p"), [link("Back to Load tests", "/ui/loadtests")]);
      if (run === null) {
        show([el("h1", "Load test " + uid), back, el("p", "No such run")]);
        return;
      }
      var summary = run.summary || {};
      var total = summary.total || {};
      var latency = total.latencyMicros || {};
      var kinds = Object.keys(total.errors || {}).sort();
      show([
        el("h1", "Load test " + (run.name || uid)),
        back,
        table(["Deployment", "Phase", "Reason", "Start", "End"],
          [[ns + "/" + dep, run.phase, run.reason, run.start, run.end]]),
        el("h2", "Total"),
        table(["Requests", "Missed", "RPS", "Statuses", "p50 µs", "p90 µs", "p99 µs",
          "max µs"], [[total.requests, total.missed, total.rps, pairs(total.statuses),
          latency.p50, latency.p90, latency.p99, latency.max]]),
        el("h2", "Error kinds"),
        table(["Kind", "Count"], kinds.map(function (kind) {
          return [kind, total.errors[kind]];
        })),
        el("h2", "Endpoints"),
        table(["Method", "Port", "Template", "Requests", "Missed", "RPS", "Statuses", "Errors",
          "p50 µs", "p90 µs", "p99 µs", "max µs"],
        (summary.endpoints || []).map(function (e) {
          var l = e.latencyMicros || {};
          return [e.method, e.port, e.template, e.requests, e.missed, e.rps, pairs(e.statuses),
            pairs(e.errors), l.p50, l.p90, l.p99, l.max];
        }))
      ]);
    }).catch(failed);
  }

  function route() {
    var parts = window.location.pathname.split("/").filter(function (p) {
      return p !== "";
    }).map(decodeURIComponent);
    if (parts.length === 1) {
      markTab("/ui/");
      listPage();
    } else if (parts.length === 2 && parts[1] === "map") {
      markTab("/ui/map");
      mapPage();
    } else if (parts.length === 2 && parts[1] === "endpoints") {
      markTab("/ui/endpoints");
      endpointsPage();
    } else if (parts.length === 2 && parts[1] === "loadtests") {
      markTab("/ui/loadtests");
      loadTestsPage();
    } else if (parts.length === 4 && parts[1] === "workloads") {
      markTab("/ui/");
      workloadPage(parts[2], parts[3]);
    } else if (parts.length === 6 && parts[1] === "workloads" && parts[4] === "loadtests") {
      markTab("/ui/loadtests");
      runPage(parts[2], parts[3], parts[5]);
    } else if (parts.length === 6 && parts[1] === "workloads" && parts[4] === "openapi") {
      markTab("/ui/");
      openapiPage(parts[2], parts[3], parts[5]);
    } else if (parts.length === 7 && parts[1] === "workloads" && parts[4] === "resources") {
      markTab("/ui/");
      resourcePage(parts[2], parts[3], parts[5], parts[6]);
    }
  }

  try {
    route();
  } catch (error) {
    failed(error);
  }
}());
