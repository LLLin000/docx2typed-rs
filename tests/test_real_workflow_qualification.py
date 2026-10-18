import json
from pathlib import Path
import sys

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from qualification.real_workflow_qualification import collect


def report(tmp_path, status, outcome, assertions):
    path = tmp_path / 'report.json'
    path.write_text(json.dumps({'schema': 'real-workflow-qualification-1',
        'workflow': 'review-handoff', 'host': {}, 'binary': {}, 'cases': [
        {'case_id': 'boundary', 'completion_status': status, 'outcome': outcome,
         'assertions': assertions, 'artifacts': {'private_text': 'secret'}, 'steps': []}]}))
    return path


def test_refusal_cannot_pass_with_failed_no_write_proof(tmp_path):
    path = report(tmp_path, 'complete', 'EXPECTED_REJECT', [{'id': 'no-write', 'pass': False}])
    with pytest.raises(ValueError, match='unproven assertions'):
        collect([path])


def test_missing_host_is_not_an_executed_outcome(tmp_path):
    path = report(tmp_path, 'not-run-no-host', None, [])
    result = collect([path])
    assert result['unexecuted'] == 1
    assert all(value == 0 for value in result['counts'].values())
    assert 'private_text' not in result['cases'][0]['artifacts']


def test_contract_failure_remains_visible(tmp_path):
    path = report(tmp_path, 'COMPLETE', 'CONTRACT_FAILURE', [{'id': 'preserved', 'pass': False}])
    result = collect([path])
    assert result['counts']['CONTRACT_FAILURE'] == 1
    assert result['cases'][0]['assertions'] == [{'id': 'preserved', 'pass': False}]
