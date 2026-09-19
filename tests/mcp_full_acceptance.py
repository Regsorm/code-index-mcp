"""Cross-platform end-to-end acceptance test for every advertised MCP tool.

The local mode creates a small synthetic 1C/BSL repository, starts an isolated
daemon and HTTP serve process, waits for the index to become ready, and calls
every tool returned by ``tools/list``.  The external mode runs the same MCP
checks against an already running server (used for the Linux container pass).

Examples:
    python tests/mcp_full_acceptance.py --binary target/release/bsl-indexer.exe
    python tests/mcp_full_acceptance.py --url http://127.0.0.1:19003/mcp
    python tests/mcp_full_acceptance.py --prepare-only \
        --workspace target/mcp-linux-fixture --config-repo-path /fixture
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

REPO_ALIAS = "acceptance"
CODE_FILE = "CommonModules/OrderService/Ext/Module.bsl"
TEXT_FILE = "README.md"

CORE_TOOLS = {
    "find_path",
    "find_symbol",
    "get_call_tree",
    "get_callees",
    "get_callers",
    "get_class",
    "get_file_summary",
    "get_function",
    "get_imports",
    "get_stats",
    "grep_body",
    "grep_code",
    "grep_text",
    "health",
    "list_files",
    "read_file",
    "search_class",
    "search_function",
    "search_text",
    "stat_file",
}

BSL_TOOLS = {
    "bsl_sql",
    "find_data_path",
    "find_path_bsl",
    "find_references",
    "get_data_links",
    "get_event_subscriptions",
    "get_form_handlers",
    "get_object_profile",
    "get_object_structure",
    "get_register_writers",
    "get_role_rights",
    "search_terms",
}

EXPECTED_TOOLS = CORE_TOOLS | BSL_TOOLS

try:
    sys.stdout.reconfigure(encoding="utf-8")  # type: ignore[attr-defined]
except Exception:
    pass


def mcp_post(
    url: str,
    payload: dict[str, Any],
    session: str | None = None,
    timeout: float = 180.0,
) -> tuple[dict[str, Any], str | None]:
    data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    }
    if session:
        headers["Mcp-Session-Id"] = session
        headers["Mcp-Protocol-Version"] = "2025-06-18"
    request = urllib.request.Request(url, data=data, headers=headers, method="POST")
    with urllib.request.urlopen(request, timeout=timeout) as response:
        session_id = response.headers.get("Mcp-Session-Id")
        raw = response.read().decode("utf-8", errors="replace")
    if not raw.strip():
        return {}, session_id
    if raw.lstrip().startswith(("event:", "data:", "id:")):
        for line in raw.splitlines():
            if not line.startswith("data:"):
                continue
            body = line[5:].strip()
            if not body:
                continue
            try:
                decoded = json.loads(body)
            except json.JSONDecodeError:
                continue
            if isinstance(decoded, dict) and ("result" in decoded or "error" in decoded):
                return decoded, session_id
        return {}, session_id
    decoded = json.loads(raw)
    if not isinstance(decoded, dict):
        raise RuntimeError("MCP response is not a JSON object")
    return decoded, session_id


def mcp_connect(url: str) -> str:
    reply, session = mcp_post(
        url,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "mcp-full-acceptance", "version": "1"},
            },
        },
    )
    if "error" in reply:
        raise RuntimeError(f"initialize failed: {reply['error']}")
    if not session:
        raise RuntimeError("server did not return mcp-session-id")
    mcp_post(
        url,
        {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
        session,
    )
    return session


def mcp_list_tools(url: str, session: str) -> list[dict[str, Any]]:
    reply, _ = mcp_post(
        url,
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
        session,
    )
    if "error" in reply:
        raise RuntimeError(f"tools/list failed: {reply['error']}")
    return list(reply["result"]["tools"])


def mcp_call(url: str, session: str, name: str, args: dict[str, Any], request_id: int) -> tuple[Any, int, str | None]:
    try:
        reply, _ = mcp_post(
            url,
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": name, "arguments": args},
            },
            session,
        )
    except (OSError, urllib.error.URLError, urllib.error.HTTPError) as exc:
        return None, 0, f"transport: {exc}"
    if "error" in reply:
        return None, 0, f"rpc: {reply['error']}"
    result = reply.get("result", {})
    if result.get("isError"):
        return result, 0, f"tool returned isError: {result}"
    content = result.get("content") or []
    text = content[0].get("text", "") if content and isinstance(content[0], dict) else ""
    size = len(text.encode("utf-8"))
    if text:
        try:
            return json.loads(text), size, None
        except json.JSONDecodeError:
            return text, size, None
    return result.get("structuredContent", result), size, None


FIXTURE_FILES = {
    "Configuration.xml": """<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject><Configuration><ChildObjects>
  <Catalog>Customer</Catalog>
  <Catalog>Product</Catalog>
  <Document>SalesOrder</Document>
  <AccumulationRegister>Stock</AccumulationRegister>
  <EventSubscription>OrderWriteSubscription</EventSubscription>
</ChildObjects></Configuration></MetaDataObject>
""",
    "Catalogs/Customer.xml": """<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <Catalog uuid="customer"><Properties><Name>Customer</Name></Properties>
    <ChildObjects><Attribute uuid="code"><Properties><Name>ExternalCode</Name>
      <Type><v8:Type>xs:string</v8:Type></Type>
    </Properties></Attribute></ChildObjects>
  </Catalog>
</MetaDataObject>
""",
    "Catalogs/Product.xml": """<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject><Catalog uuid="product"><Properties><Name>Product</Name></Properties></Catalog></MetaDataObject>
""",
    "Documents/SalesOrder.xml": """<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core"
                xmlns:xr="http://v8.1c.ru/8.3/xcf/readable"
                xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Document uuid="sales-order">
    <Properties><Name>SalesOrder</Name><Posting>Allow</Posting>
      <RegisterRecords>
        <xr:Item xsi:type="xr:MDObjectRef">AccumulationRegister.Stock</xr:Item>
      </RegisterRecords>
    </Properties>
    <ChildObjects>
      <Attribute uuid="customer-ref"><Properties><Name>Customer</Name>
        <Type><v8:Type>cfg:CatalogRef.Customer</v8:Type></Type>
      </Properties></Attribute>
      <TabularSection uuid="lines"><Properties><Name>Lines</Name></Properties>
        <ChildObjects><Attribute uuid="product-ref"><Properties><Name>Product</Name>
          <Type><v8:Type>cfg:CatalogRef.Product</v8:Type></Type>
        </Properties></Attribute></ChildObjects>
      </TabularSection>
    </ChildObjects>
  </Document>
</MetaDataObject>
""",
    "AccumulationRegisters/Stock.xml": """<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <AccumulationRegister uuid="stock"><Properties><Name>Stock</Name></Properties>
    <ChildObjects>
      <Dimension uuid="product"><Properties><Name>Product</Name>
        <Type><v8:Type>cfg:CatalogRef.Product</v8:Type></Type>
      </Properties></Dimension>
      <Resource uuid="quantity"><Properties><Name>Quantity</Name>
        <Type><v8:Type>xs:decimal</v8:Type></Type>
      </Properties></Resource>
    </ChildObjects>
  </AccumulationRegister>
</MetaDataObject>
""",
    "EventSubscriptions/OrderWriteSubscription.xml": """<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns:v8="http://v8.1c.ru/8.3/data/core">
  <EventSubscription><Properties><Name>OrderWriteSubscription</Name>
    <Source><Type><v8:Type>cfg:DocumentRef.SalesOrder</v8:Type></Type></Source>
    <Event>BeforeWrite</Event><Handler>OrderService.OnOrderWrite</Handler>
  </Properties></EventSubscription>
</MetaDataObject>
""",
    "Documents/SalesOrder/Forms/OrderForm/Ext/Form.xml": """<?xml version="1.0" encoding="UTF-8"?>
<Form uuid="order-form"><Events>
  <Event name="OnOpen">OnOpen</Event>
</Events></Form>
""",
    "Documents/SalesOrder/Forms/OrderForm/Ext/Form/Module.bsl": """&НаСервере
Процедура OnOpen()
    OrderService.ProcessOrder();
КонецПроцедуры
""",
    CODE_FILE: """Функция CalculateTotal(Quantity, Price) Экспорт
    Возврат Quantity * Price;
КонецФункции

Процедура ProcessOrder() Экспорт
    Total = CalculateTotal(2, 10);
    SaveOrder();
КонецПроцедуры

Процедура SaveOrder() Экспорт
    Сообщить("Acceptance order saved");
КонецПроцедуры

Процедура OnOrderWrite(Source, Refusal) Экспорт
    ProcessOrder();
КонецПроцедуры
""",
    "Documents/SalesOrder/Ext/ObjectModule.bsl": """Процедура BeforeWrite(Cancel)
    OrderService.ProcessOrder();
КонецПроцедуры
""",
    "Roles/Manager/Ext/Rights.xml": """<?xml version="1.0" encoding="UTF-8"?>
<Rights><object><name>Document.SalesOrder</name>
  <right><name>Read</name><value>true</value></right>
  <right><name>Update</name><value>true</value></right>
</object><object><name>Catalog.Customer</name>
  <right><name>Read</name><value>true</value></right>
</object></Rights>
""",
    TEXT_FILE: """# MCP acceptance fixture

This repository validates the acceptance pipeline and text search.
""",
}


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def prepare_fixture(workspace: Path, config_repo_path: str | None = None) -> tuple[Path, Path]:
    repo = workspace / "repo"
    home = workspace / "home"
    repo.mkdir(parents=True, exist_ok=True)
    home.mkdir(parents=True, exist_ok=True)
    for relative, content in FIXTURE_FILES.items():
        write_text(repo / relative, content)

    configured = config_repo_path or str(repo.resolve())
    daemon_toml = (
        "[daemon]\n"
        "http_host = \"127.0.0.1\"\n"
        "http_port = 0\n"
        "log_level = \"info\"\n\n"
        "[[paths]]\n"
        f"path = {json.dumps(configured)}\n"
        f"alias = \"{REPO_ALIAS}\"\n"
        "language = \"bsl\"\n"
    )
    write_text(home / "daemon.toml", daemon_toml)
    return repo, home


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def creation_flags() -> int:
    if os.name == "nt":
        return int(getattr(subprocess, "CREATE_NO_WINDOW", 0))
    return 0


def tail(path: Path, lines: int = 40) -> str:
    try:
        return "\n".join(path.read_text(encoding="utf-8", errors="replace").splitlines()[-lines:])
    except OSError:
        return "<log unavailable>"


def start_process(command: list[str], env: dict[str, str], log_path: Path) -> tuple[subprocess.Popen[bytes], Any]:
    log = log_path.open("wb")
    process = subprocess.Popen(
        command,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=subprocess.STDOUT,
        creationflags=creation_flags(),
    )
    return process, log


def stop_process(process: subprocess.Popen[bytes] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=15)


def wait_for_file(path: Path, process: subprocess.Popen[bytes], timeout: float, log_path: Path) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file():
            return
        code = process.poll()
        if code is not None:
            raise RuntimeError(f"process exited with code {code}\n{tail(log_path)}")
        time.sleep(0.2)
    raise RuntimeError(f"timed out waiting for {path}\n{tail(log_path)}")


def wait_for_port(port: int, process: subprocess.Popen[bytes], timeout: float, log_path: Path) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        code = process.poll()
        if code is not None:
            raise RuntimeError(f"serve exited with code {code}\n{tail(log_path)}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return
        except OSError:
            time.sleep(0.2)
    raise RuntimeError(f"timed out waiting for HTTP port {port}\n{tail(log_path)}")


def unwrap(value: Any) -> Any:
    current = value
    for _ in range(4):
        if isinstance(current, dict) and "result" in current:
            current = current["result"]
        else:
            break
    return current


def response_error(value: Any) -> str | None:
    if isinstance(value, dict):
        status = value.get("status")
        if status in {"error", "daemon_offline", "not_started", "indexing", "federation_error"}:
            return f"status={status}: {value.get('message') or value.get('error') or ''}"
        error = value.get("error")
        if isinstance(error, str) and error:
            return error
        for child in value.values():
            found = response_error(child)
            if found:
                return found
    elif isinstance(value, list):
        for child in value:
            found = response_error(child)
            if found:
                return found
    return None


def is_ready(health: Any) -> bool:
    if not isinstance(health, dict):
        return False
    if health.get("daemon", {}).get("status") != "online":
        return False
    repos = health.get("repos") or []
    return any(
        item.get("repo") == REPO_ALIAS
        and item.get("path_status", {}).get("status") == "ready"
        for item in repos
        if isinstance(item, dict)
    )


def connect_when_ready(url: str, timeout: float) -> str:
    deadline = time.monotonic() + timeout
    last_error = "server did not answer"
    while time.monotonic() < deadline:
        try:
            session = mcp_connect(url)
            payload, _, error = mcp_call(url, session, "health", {}, 3)
            if error:
                last_error = error
            elif is_ready(unwrap(payload)):
                return session
            else:
                last_error = f"not ready: {payload!r}"
        except Exception as exc:  # server startup and MCP handshake failures
            last_error = str(exc)
        time.sleep(0.5)
    raise RuntimeError(f"MCP repository did not become ready: {last_error}")


def tool_arguments() -> dict[str, dict[str, Any]]:
    repo = REPO_ALIAS
    return {
        "health": {},
        "get_stats": {"repo": repo},
        "list_files": {"repo": repo, "limit": 20},
        "stat_file": {"repo": repo, "path": CODE_FILE},
        "read_file": {"repo": repo, "path": CODE_FILE, "line_start": 1, "line_end": 40},
        "search_function": {"repo": repo, "query": "ProcessOrder", "limit": 10},
        "search_class": {"repo": repo, "query": "AbsentClass", "limit": 10},
        "search_text": {"repo": repo, "query": "acceptance pipeline", "limit": 10},
        "find_symbol": {"repo": repo, "name": "ProcessOrder", "language": "bsl"},
        "get_function": {"repo": repo, "name": "ProcessOrder", "language": "bsl"},
        "get_class": {"repo": repo, "name": "AbsentClass", "language": "bsl"},
        "get_callers": {"repo": repo, "function_name": "CalculateTotal", "limit": 10},
        "get_callees": {"repo": repo, "function_name": "ProcessOrder", "limit": 10},
        "find_path": {"repo": repo, "from": "ProcessOrder", "to": "CalculateTotal", "max_depth": 3},
        "get_call_tree": {"repo": repo, "root": "ProcessOrder", "direction": "callees", "max_depth": 3},
        "get_imports": {"repo": repo, "module": "OrderService", "language": "bsl", "limit": 10},
        "get_file_summary": {"repo": repo, "path": CODE_FILE},
        "grep_body": {"repo": repo, "pattern": "CalculateTotal", "language": "bsl", "limit": 10},
        "grep_code": {"repo": repo, "pattern": "Acceptance order", "language": "bsl", "limit": 10},
        "grep_text": {"repo": repo, "pattern": "acceptance pipeline", "path_glob": "**/*.md", "limit": 10},
        "get_object_structure": {"repo": repo, "full_name": "Document.SalesOrder"},
        "get_form_handlers": {"repo": repo, "owner_full_name": "Document.SalesOrder"},
        "get_event_subscriptions": {"repo": repo, "limit": 10},
        "find_path_bsl": {"repo": repo, "from": "ProcessOrder", "to": "CalculateTotal", "max_depth": 3},
        "get_data_links": {"repo": repo, "object": "Document.SalesOrder", "direction": "both", "depth": 1},
        "find_data_path": {"repo": repo, "from": "Document.SalesOrder", "to": "Catalog.Customer"},
        "get_register_writers": {"repo": repo, "object": "AccumulationRegister.Stock"},
        "get_role_rights": {"repo": repo, "object": "Document.SalesOrder"},
        "search_terms": {"repo": repo, "query": "process order", "limit": 10},
        "bsl_sql": {"repo": repo, "sql": "SELECT COUNT(*) AS count FROM metadata_objects"},
        "get_object_profile": {"repo": repo, "full_name": "Document.SalesOrder"},
        "find_references": {"repo": repo, "object": "Catalog.Customer", "limit": 10},
    }


def nonempty(value: Any) -> bool:
    body = unwrap(value)
    if body is None:
        return False
    if isinstance(body, (str, list, dict)):
        return len(body) > 0
    return True


def validate_anchor(tool: str, payload: Any) -> str | None:
    body = unwrap(payload)
    if tool == "health" and not is_ready(body):
        return "health does not report an online daemon and ready repository"
    if tool in {"search_function", "get_function", "find_symbol", "get_callees", "get_callers", "find_path", "get_file_summary"}:
        if "ProcessOrder" not in json.dumps(payload, ensure_ascii=False) and "CalculateTotal" not in json.dumps(payload, ensure_ascii=False):
            return "expected function/call-graph anchor is missing"
    if tool in {"read_file", "grep_body", "grep_code"}:
        if "ProcessOrder" not in json.dumps(payload, ensure_ascii=False) and "CalculateTotal" not in json.dumps(payload, ensure_ascii=False) and "Acceptance order" not in json.dumps(payload, ensure_ascii=False):
            return "expected code anchor is missing"
    if tool in {"get_object_structure", "get_object_profile", "get_data_links", "find_data_path", "get_register_writers", "get_role_rights", "find_references"}:
        if not any(anchor in json.dumps(payload, ensure_ascii=False) for anchor in ("SalesOrder", "Customer", "Stock", "Manager")):
            return "expected metadata anchor is missing"
    if tool == "get_form_handlers" and "OnOpen" not in json.dumps(payload, ensure_ascii=False):
        return "form handler anchor is missing"
    if tool == "get_event_subscriptions" and "OrderWriteSubscription" not in json.dumps(payload, ensure_ascii=False):
        return "event subscription anchor is missing"
    if tool in {"list_files", "get_stats", "search_text", "grep_text", "bsl_sql"} and not nonempty(payload):
        return "expected a non-empty response"
    return None


def run_acceptance(url: str, timeout: float) -> dict[str, Any]:
    session = connect_when_ready(url, timeout)
    advertised = mcp_list_tools(url, session)
    names = {str(tool["name"]) for tool in advertised}
    missing = sorted(EXPECTED_TOOLS - names)
    extra = sorted(names - EXPECTED_TOOLS)
    if missing or extra:
        raise RuntimeError(f"tools/list mismatch: missing={missing}, extra={extra}")

    arguments = tool_arguments()
    unmapped = sorted(names - arguments.keys())
    if unmapped:
        raise RuntimeError(f"no acceptance arguments for advertised tools: {unmapped}")

    results: list[dict[str, Any]] = []
    failures: list[str] = []
    for index, name in enumerate(sorted(names), start=1):
        started = time.monotonic()
        payload, size, error = mcp_call(url, session, name, arguments[name], 100 + index)
        duration_ms = round((time.monotonic() - started) * 1000)
        content_error = response_error(payload) if not error else None
        anchor_error = validate_anchor(name, payload) if not error and not content_error else None
        failure = error or content_error or anchor_error
        results.append(
            {
                "index": index,
                "tool": name,
                "ok": failure is None,
                "duration_ms": duration_ms,
                "response_bytes": size,
                "error": failure,
            }
        )
        state = "PASS" if failure is None else "FAIL"
        print(f"[{index:02d}/{len(names)}] {state:4} {name:26} {duration_ms:6d} ms {size:8d} B")
        if failure:
            failures.append(f"{name}: {failure}")

    report = {
        "platform": platform.platform(),
        "python": sys.version.split()[0],
        "url": url,
        "advertised_tools": sorted(names),
        "tool_count": len(names),
        "passed": len(names) - len(failures),
        "failed": len(failures),
        "results": results,
        "failures": failures,
    }
    if failures:
        raise AcceptanceFailure(report)
    return report


class AcceptanceFailure(RuntimeError):
    def __init__(self, report: dict[str, Any]):
        self.report = report
        super().__init__("; ".join(report["failures"]))


def write_report(path: Path, report: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"report: {path.resolve()}")


def safe_remove(path: Path, target_root: Path) -> None:
    resolved = path.resolve()
    root = target_root.resolve()
    try:
        resolved.relative_to(root)
    except ValueError as exc:
        raise RuntimeError(f"refusing to remove path outside target: {resolved}") from exc
    shutil.rmtree(resolved, ignore_errors=True)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, help="bsl-indexer binary for isolated local mode")
    parser.add_argument("--url", help="already running MCP URL; skips process launch")
    parser.add_argument("--workspace", type=Path, help="persistent fixture workspace")
    parser.add_argument("--config-repo-path", help="path written to daemon.toml (for container mounts)")
    parser.add_argument("--prepare-only", action="store_true", help="create fixture/config and exit")
    parser.add_argument("--keep", action="store_true", help="keep auto-created local workspace")
    parser.add_argument("--port", type=int, help="HTTP port for isolated local mode")
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("--report", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path(__file__).resolve().parents[1]
    target_root = repo_root / "target"
    target_root.mkdir(exist_ok=True)
    auto_workspace = args.workspace is None
    workspace = args.workspace or Path(tempfile.mkdtemp(prefix="mcp-acceptance-", dir=target_root))
    workspace = workspace.resolve()

    if args.prepare_only:
        repo, home = prepare_fixture(workspace, args.config_repo_path)
        print(json.dumps({"workspace": str(workspace), "repo": str(repo), "home": str(home)}, indent=2))
        return 0

    report_path = args.report or target_root / f"mcp-acceptance-{platform.system().lower()}.json"
    daemon: subprocess.Popen[bytes] | None = None
    serve: subprocess.Popen[bytes] | None = None
    daemon_log_handle = None
    serve_log_handle = None

    try:
        if args.url:
            url = args.url
        else:
            if args.binary is None:
                raise RuntimeError("--binary is required unless --url or --prepare-only is used")
            binary = args.binary.resolve()
            if not binary.is_file():
                raise RuntimeError(f"binary does not exist: {binary}")
            _, home = prepare_fixture(workspace)
            env = os.environ.copy()
            env["CODE_INDEX_HOME"] = str(home)
            env.setdefault("RUST_LOG", "info")
            daemon_log = workspace / "daemon.log"
            serve_log = workspace / "serve.log"
            daemon, daemon_log_handle = start_process([str(binary), "daemon", "run"], env, daemon_log)
            wait_for_file(home / "daemon.json", daemon, args.timeout, daemon_log)

            port = args.port or free_port()
            serve, serve_log_handle = start_process(
                [
                    str(binary),
                    "serve",
                    "--transport",
                    "http",
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(port),
                    "--config",
                    str(home / "daemon.toml"),
                ],
                env,
                serve_log,
            )
            wait_for_port(port, serve, args.timeout, serve_log)
            url = f"http://127.0.0.1:{port}/mcp"

        try:
            report = run_acceptance(url, args.timeout)
        except AcceptanceFailure as exc:
            write_report(report_path, exc.report)
            raise
        write_report(report_path, report)
        print(f"PASS: {report['passed']}/{report['tool_count']} advertised MCP tools")
        return 0
    finally:
        stop_process(serve)
        stop_process(daemon)
        if serve_log_handle is not None:
            serve_log_handle.close()
        if daemon_log_handle is not None:
            daemon_log_handle.close()
        if auto_workspace and not args.keep:
            safe_remove(workspace, target_root)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        raise SystemExit(1)
