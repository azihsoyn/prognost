//! Go and Python end to end: a small repository committed, changed in
//! the working tree, planned and assessed as `prognost plan | assess`
//! would.

use std::path::Path;
use std::process::Command;

use prognost::impact::Change;
use prognost::plan::PlanReport;
use prognost::rev::Rev;

fn git(root: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

/// `base` committed, `change` written over it in the working tree: the
/// plan of that change.
fn plan(base: &[(&str, &str)], change: &[(&str, &str)]) -> (tempfile::TempDir, PlanReport) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    write(root, base);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    write(root, change);
    let sha = prognost::origin::commit_sha(root, "HEAD").unwrap();
    let (base_rev, head_rev) = (Rev::commit(sha), Rev::working());
    let base_ws = prognost::workspace::discover(root, &base_rev).unwrap();
    let head_ws = prognost::workspace::discover(root, &head_rev).unwrap();
    let mut app =
        prognost::flow_tui::App::from_changes(root.into(), base_rev, head_rev, base_ws, head_ws)
            .unwrap();
    let report = app.plan_report(8).unwrap();
    (dir, report)
}

fn names(report: &PlanReport, change: Change) -> Vec<String> {
    let mut v: Vec<String> = report
        .functions
        .iter()
        .filter(|f| f.change == change)
        .map(|f| format!("{}:{}", f.path, f.name))
        .collect();
    v.sort();
    v
}

fn rules_found(dir: &Path, report: &PlanReport) -> Vec<String> {
    let a = prognost::assess::assess(dir, report, &[]).unwrap();
    a.findings.into_iter().map(|f| f.rule).collect()
}

const GO: &[(&str, &str)] = &[
    ("go.mod", "module example.com/shop\n\ngo 1.22\n"),
    (
        "internal/db/pool.go",
        r#"package db

type Pool struct{}

func Open(size int) *Pool { return &Pool{} }

func (p *Pool) Query(q string, args ...any) error {
	return p.exec(q)
}

func (p *Pool) exec(q string) error { return nil }
"#,
    ),
    (
        "internal/orders/service.go",
        r#"package orders

import "example.com/shop/internal/db"

type Service struct{ pool *db.Pool }

func (s *Service) Charge(ids []string) error {
	for _, id := range ids {
		if err := s.pool.Query("UPDATE orders SET paid = true WHERE id = $1", id); err != nil {
			return err
		}
	}
	return nil
}

func New(pool *db.Pool) *Service { return &Service{pool: pool} }
"#,
    ),
    (
        "internal/orders/handler.go",
        r#"package orders

import "net/http"

func Routes(mux *http.ServeMux, s *Service) {
	mux.HandleFunc("/orders/charge", func(w http.ResponseWriter, r *http.Request) {
		_ = s.Charge(nil)
	})
}
"#,
    ),
    (
        "cmd/api/main.go",
        r#"package main

import (
	"net/http"

	shopdb "example.com/shop/internal/db"
	"example.com/shop/internal/orders"
)

func main() {
	mux := http.NewServeMux()
	orders.Routes(mux, orders.New(shopdb.Open(4)))
	http.ListenAndServe(":8080", mux)
}
"#,
    ),
];

#[test]
fn go_reaches_through_methods_packages_and_routes() {
    let (dir, report) = plan(
        GO,
        &[(
            "internal/db/pool.go",
            r#"package db

type Pool struct{}

func Open(size int) *Pool { return &Pool{} }

func (p *Pool) Query(q string, args ...any) error {
	for i := 0; i < 3; i++ {
		defer p.release(i)
	}
	return p.exec(q)
}

func (p *Pool) release(i int) {}

func (p *Pool) exec(q string) error { return nil }
"#,
        )],
    );
    assert_eq!(
        names(&report, Change::Changed),
        vec!["internal/db/pool.go:Query"]
    );
    assert_eq!(
        names(&report, Change::Added),
        vec!["internal/db/pool.go:release"]
    );
    let upstream = names(&report, Change::Unchanged);
    for f in [
        "internal/orders/service.go:Charge",
        "internal/orders/handler.go:Routes",
        "cmd/api/main.go:main",
    ] {
        assert!(upstream.iter().any(|u| u == f), "{f} not in {upstream:?}");
    }
    let route = report
        .functions
        .iter()
        .find(|f| f.path == "internal/orders/handler.go" && f.route.is_some())
        .expect("the handler literal");
    assert_eq!(route.route.as_deref(), Some("ANY /orders/charge"));
    let query = report
        .summary
        .symbols
        .iter()
        .find(|s| s.id.ends_with("Query"))
        .unwrap();
    assert!(query.public, "exported and called from another package");
    assert!(rules_found(dir.path(), &report).contains(&"defer-in-loop".to_string()));
}

const PY: &[(&str, &str)] = &[
    ("pyproject.toml", "[project]\nname = \"shop\"\n"),
    ("src/shop/__init__.py", ""),
    ("src/shop/db/__init__.py", "from .pool import query\n"),
    (
        "src/shop/db/pool.py",
        "_pool = None\n\nasync def query(sql, *args):\n    return await _pool.fetch(sql, *args)\n",
    ),
    (
        "src/shop/billing.py",
        "from shop.db import query\n\n\nasync def charge(ids):\n    await query(\"BEGIN\")\n    for i in ids:\n        pass\n    return len(ids)\n",
    ),
    (
        "src/shop/api/routes.py",
        r#"from fastapi import APIRouter

from .. import billing
from ..db import pool

router = APIRouter()


@router.post("/charge")
async def charge_orders(body: dict):
    return await billing.charge(body["ids"])


@router.get("/orders")
async def list_orders():
    return await pool.query("SELECT * FROM orders")
"#,
    ),
    (
        "tests/test_billing.py",
        "from shop.billing import charge\n\ndef test_charge():\n    charge([])\n",
    ),
];

#[test]
fn python_reaches_through_relative_imports_and_package_reexports() {
    let (dir, report) = plan(
        PY,
        &[(
            "src/shop/billing.py",
            "from shop.db import query\n\n\nasync def charge(ids):\n    await query(\"BEGIN\")\n    for i in ids:\n        await query(\"UPDATE invoices SET charged = true WHERE id = $1\", i)\n    return len(ids)\n",
        )],
    );
    assert_eq!(
        names(&report, Change::Changed),
        vec!["src/shop/billing.py:charge"]
    );
    let upstream = names(&report, Change::Unchanged);
    // A route handler goes by its route.
    assert!(
        upstream.contains(&"src/shop/api/routes.py:POST /charge".to_string()),
        "{upstream:?}"
    );
    assert!(
        !upstream.iter().any(|u| u.starts_with("tests/")),
        "{upstream:?}"
    );
    assert!(rules_found(dir.path(), &report).contains(&"python-await-in-loop".to_string()));

    // The other way in: `pool.query` after `from ..db import pool`, and
    // `query` re-exported by the package's `__init__.py`.
    let (_dir, report) = plan(
        PY,
        &[(
            "src/shop/db/pool.py",
            "_pool = None\n\nasync def query(sql, *args):\n    return list(await _pool.fetch(sql, *args))\n",
        )],
    );
    let upstream = names(&report, Change::Unchanged);
    for f in [
        "src/shop/api/routes.py:GET /orders",
        "src/shop/billing.py:charge",
    ] {
        assert!(upstream.iter().any(|u| u == f), "{f} not in {upstream:?}");
    }
}

#[test]
fn go_calls_through_an_interface_reach_its_implementations_as_inferred() {
    let base: &[(&str, &str)] = &[
        ("go.mod", "module example.com/shop\n\ngo 1.22\n"),
        (
            "orders/service.go",
            "package orders\n\ntype Store interface {\n\tSave(id string) error\n}\n\ntype Service struct{ store Store }\n\nfunc (s *Service) Checkout(id string) error {\n\treturn s.store.Save(id)\n}\n",
        ),
        (
            "postgres/store.go",
            "package postgres\n\ntype PgStore struct{}\n\nfunc (p *PgStore) Save(id string) error { return nil }\n",
        ),
        (
            "memory/store.go",
            "package memory\n\ntype MemStore struct{}\n\nfunc (m *MemStore) Save(id string) error { return nil }\n",
        ),
    ];
    let (_dir, report) = plan(
        base,
        &[(
            "postgres/store.go",
            "package postgres\n\ntype PgStore struct{}\n\nfunc (p *PgStore) Save(id string) error {\n\tif id == \"\" {\n\t\treturn nil\n\t}\n\treturn nil\n}\n",
        )],
    );
    let call = report
        .calls
        .iter()
        .find(|c| c.callee == "postgres/store.go::Save")
        .expect("a call into the changed method");
    assert_eq!(call.caller, "orders/service.go::Checkout");
    assert!(call.inferred);
    assert_eq!(call.line, Some(10));
}

#[test]
fn python_calls_on_typed_instances_reach_the_method() {
    let base: &[(&str, &str)] = &[
        ("pyproject.toml", "[project]\nname = \"app\"\n"),
        ("app/__init__.py", ""),
        (
            "app/repo.py",
            "class OrderRepo:\n    def save(self, order):\n        return order\n",
        ),
        (
            "app/service.py",
            r#"from app.repo import OrderRepo


class Checkout:
    def __init__(self, repo: OrderRepo):
        self.repo = repo

    def run(self, order):
        self.repo.save(order)


def nightly(db):
    repo = OrderRepo()
    repo.save(db)


def save(x):
    return x
"#,
        ),
        (
            "app/other.py",
            "from app.service import save\n\n\ndef unrelated():\n    save(1)\n",
        ),
    ];
    let (_dir, report) = plan(
        base,
        &[(
            "app/repo.py",
            "class OrderRepo:\n    def save(self, order):\n        return [order]\n",
        )],
    );
    let upstream = names(&report, Change::Unchanged);
    assert_eq!(
        upstream,
        vec!["app/service.py:nightly", "app/service.py:run"],
        "a bare save() elsewhere is another function"
    );
    assert!(report.calls.iter().all(|c| !c.inferred));
}
