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

  function placeholder(title, text) {
    show([el("h1", title), el("p", text, { class: "placeholder" })]);
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

  function route() {
    var parts = window.location.pathname.split("/").filter(function (p) {
      return p !== "";
    }).map(decodeURIComponent);
    if (parts.length === 1) {
      markTab("/ui/");
      listPage();
    } else if (parts.length === 2 && parts[1] === "map") {
      markTab("/ui/map");
      placeholder("Map", "The map of calls between deployments is not available yet.");
    } else if (parts.length === 2 && parts[1] === "endpoints") {
      markTab("/ui/endpoints");
      placeholder("Endpoints", "Pick a deployment in the Workloads tab to see its endpoints.");
    } else if (parts.length === 2 && parts[1] === "loadtests") {
      markTab("/ui/loadtests");
      placeholder("Load tests", "The runs of every deployment are not available here yet.");
    } else if (parts.length === 4) {
      markTab("/ui/");
      workloadPage(parts[2], parts[3]);
    } else if (parts.length === 6) {
      markTab("/ui/");
      openapiPage(parts[2], parts[3], parts[5]);
    } else if (parts.length === 7) {
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
