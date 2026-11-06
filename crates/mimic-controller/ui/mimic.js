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

  function listPage() {
    api("/v1/workloads").then(function (data) {
      var workloads = (data && data.workloads) || [];
      if (workloads.length === 0) {
        show([el("h1", "Workloads"), el("p", "No data")]);
        return;
      }
      show([el("h1", "Workloads"), table(
        ["Namespace", "Deployment", "Endpoints", "Exchanges", "Last push"],
        workloads.map(function (w) {
          return [w.namespace, link(w.deployment, workloadPath(w.namespace, w.deployment)),
            w.endpoints, w.exchanges, w.last_push];
        })
      )]);
    }).catch(failed);
  }

  function workloadPage(ns, dep) {
    Promise.all([
      api(apiPath(ns, dep)),
      api(apiPath(ns, dep, "/endpoints")),
      api(apiPath(ns, dep, "/openapi"))
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
        return add(el("li"), [
          link(doc.id, workloadPath(ns, dep) + "/openapi/" + enc(doc.id)),
          document.createTextNode(" (" + doc.endpoints + " endpoints)")
        ]);
      }));
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
          ["Direction", "Port", "Method", "Template", "Exchanges", "Statuses",
            "p50 ms", "p90 ms", "p99 ms", "Last seen"],
          endpoints.map(function (e) {
            var latency = e.latency_ms || {};
            return [e.direction, e.port, e.method, e.template, e.exchanges, pairs(e.statuses),
              latency.p50, latency.p90, latency.p99, e.last_seen];
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
      listPage();
    } else if (parts.length === 4) {
      workloadPage(parts[2], parts[3]);
    } else if (parts.length === 6) {
      openapiPage(parts[2], parts[3], parts[5]);
    }
  }

  try {
    route();
  } catch (error) {
    failed(error);
  }
}());
