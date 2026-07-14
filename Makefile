# srg_sim developer tasks.
#
# Uses the shared virtualenv at ~/data/stars/venv (do NOT recreate it).
# Override with:  make VENV=/path/to/venv <target>

VENV ?= $(HOME)/data/stars/venv
PY   := $(VENV)/bin/python
PIP  := $(VENV)/bin/pip

.DEFAULT_GOAL := help

.PHONY: help dev lint fmt typecheck test check docs docs-clean precommit todo clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

dev: ## Install srg_sim + dev deps into the venv (editable)
	$(PIP) install -e ".[dev]"

lint: ## Lint with ruff
	$(VENV)/bin/ruff check srg_sim tests

fmt: ## Auto-format with ruff
	$(VENV)/bin/ruff format srg_sim tests
	$(VENV)/bin/ruff check --fix srg_sim tests

typecheck: ## Static type check with mypy
	$(PY) -m mypy srg_sim

test: ## Run the test suite
	$(PY) -m pytest

check: lint typecheck test ## Lint + typecheck + test (what CI runs)

docs: ## Build the Sphinx docs (HTML)
	$(VENV)/bin/sphinx-build -b html docs docs/_build/html

docs-clean: ## Remove built docs
	rm -rf docs/_build

precommit: ## Run all pre-commit hooks against every file
	$(VENV)/bin/pre-commit run --all-files

todo: ## Show the current task list (todo-sqlite-cli)
	todo-sqlite-cli list

clean: docs-clean ## Remove caches and build artifacts
	rm -rf .pytest_cache .mypy_cache .ruff_cache build dist *.egg-info
	find . -type d -name __pycache__ -prune -exec rm -rf {} +
