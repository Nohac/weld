// Run with node --test scripts/test-godot-hoist-charts.cjs; no browser/packages needed.
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {join} = require("node:path");
const {test} = require("node:test");
const {runInNewContext} = require("node:vm");

const context = {document: {getElementById: () => ({})}};
runInNewContext(readFileSync(join(__dirname, "godot-hoist-charts.js"), "utf8"), context);
const stats = (...args) => JSON.parse(JSON.stringify(context.chartStatistics(...args)));

test("rates are duration weighted, preserve zeros, and exclude gaps", () => {
    const points = [[0, 1, [10, 0]], [1, 4, [30, 2]], [8, 10, [0, 0]]];
    assert.deepEqual(stats(points, 0, 10, "interval", 2), {
        averages: [100 / 6, 1], totals: [100, 6],
    });
    assert.deepEqual(stats(points, 2, 9, "interval", 2), {
        averages: [20, 4 / 3], totals: [60, 4],
    });
    assert.deepEqual(stats(points, 5, 7, "interval", 2), {
        averages: [null, null], totals: [null, null],
    });
});

test("samples and recorded maxima use timestamps, not visual marker widths", () => {
    const points = [[0, 1, [10]], [1, 4, [30]], [8, 10, [0]]];
    for (const kind of ["sample", "maxima"]) {
        assert.deepEqual(stats(points, 0, 10, kind, 1), {averages: [40 / 3], totals: [null]});
        assert.deepEqual(stats(points, 2, 9, kind, 1), {averages: [30], totals: [null]});
        assert.deepEqual(stats(points, 2, 3, kind, 1), {averages: [null], totals: [null]});
    }
});

test("missing series values are excluded rather than counted as zero", () => {
    assert.deepEqual(stats([[0, 1, [null]], [1, 3, [6]]], 0, 3, "interval", 1), {
        averages: [6], totals: [12],
    });
});
