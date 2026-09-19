// Embedded into each report; only uPlot itself is loaded from the pinned CDN.
function chartStatistics(measurements, minimum, maximum, kind, count) {
    const sums = Array(count).fill(0);
    const weights = Array(count).fill(0);
    for (const [begin, end, values] of measurements) {
        // Sample spans are only visual markers. Their actual observation time
        // is the right endpoint; do not weight them by marker width.
        const weight = kind === "interval"
            ? Math.max(0, Math.min(end, maximum) - Math.max(begin, minimum))
            : Number(end >= minimum && end <= maximum);
        if (weight === 0) continue;
        values.forEach((value, index) => {
            if (value == null) return;
            sums[index] += value * weight;
            weights[index] += weight;
        });
    }
    return {
        averages: sums.map((sum, index) => weights[index] > 0 ? sum / weights[index] : null),
        totals: sums.map((sum, index) => kind === "interval" && weights[index] > 0 ? sum : null),
    };
}

(() => {
    const status = document.getElementById("chart-status");
    if (typeof uPlot === "undefined") {
        status.textContent = "Charts unavailable: could not load uPlot from the CDN. Check Internet access and reload this report.";
        return;
    }
    const colors = ["#60cfa5", "#efae5b", "#ed6e78", "#a995eb", "#79b7ed", "#bbbbbb", "#f193cd"];
    const plots = [];
    const lockYAxis = document.getElementById("lock-y-axis");
    const fullRange = {min: 0, max: 1};
    function setRange(range) {
        for (const plot of plots) plot.setScale("x", range);
    }
    try {
        for (const element of document.querySelectorAll(".chart-data")) {
            const {labels, data, end, unit, measurements, average_kind} = JSON.parse(element.textContent);
            const container = element.previousElementSibling;
            const wholeStatistics = chartStatistics(measurements, 0, end, average_kind, labels.length);
            const wholeAverages = wholeStatistics.averages;
            let visibleAverages = wholeAverages;
            const wholeMaximum = data.slice(1).reduce((max, values) =>
                values.reduce((max, value) => Math.max(max, value ?? 0), max), 0);
            const format = value => value == null ? "—" : `${value.toFixed(3)} ${unit}`;
            fullRange.max = end;
            const plot = new uPlot({
                width: container.clientWidth,
                height: 240,
                padding: [12, 16, 0, 0],
                scales: {
                    x: {time: false, auto: false, min: 0, max: end},
                    y: {range: (plot, min, max) => {
                        const baselineMaximum = wholeAverages.reduce((max, value, index) =>
                            plot.series[index + 1].show ? Math.max(max, value ?? 0) : max, 0);
                        return [0, Math.max(1, (lockYAxis.checked ? wholeMaximum
                            : Math.max(max ?? 0, baselineMaximum)) * 1.1)];
                    }},
                },
                axes: [
                    {stroke: "#aab8c7", grid: {stroke: "#33404d"}, values: (_, values) => values.map(value => `${value.toFixed(1)}s`)},
                    {stroke: "#aab8c7", grid: {stroke: "#33404d"}, size: 70},
                ],
                cursor: {
                    y: false,
                    drag: {x: true, y: false, setScale: false},
                    sync: {key: "weld-run", setSeries: false},
                    // Use the interval under the cursor, not a future sample
                    // or a nearby non-null value across a diagnostic gap.
                    dataIdx: (plot, series, index) => {
                        const time = plot.posToVal(plot.cursor.left, "x");
                        const interval = data[0][index] > time ? index - 1 : index;
                        return interval < 0 ? null : interval;
                    },
                },
                series: [
                    {label: "Time", value: (_, value) => value == null ? "—" : `${value.toFixed(2)}s`},
                    ...labels.map((label, index) => ({
                        label, stroke: colors[index % colors.length], width: 1.5,
                        paths: uPlot.paths.stepped({align: 1}),
                        points: {show: false}, spanGaps: false,
                        values: (plot, seriesIndex, dataIndex) => {
                            const values = {
                                Hover: format(dataIndex == null ? null : data[index + 1][dataIndex]),
                                "Visible avg": format(visibleAverages[index]),
                                "Run avg": format(wholeAverages[index]),
                            };
                            if (average_kind === "interval") {
                                const total = wholeStatistics.totals[index];
                                values["Run total"] = total == null ? "—"
                                    : `${total.toLocaleString(undefined, {maximumFractionDigits: 0})} ${unit.replace(/\/s$/, "")}`;
                            }
                            return values;
                        },
                    })),
                ],
                hooks: {
                    setScale: [(plot, scale) => {
                        if (scale !== "x") return;
                        visibleAverages = chartStatistics(measurements, plot.scales.x.min,
                            plot.scales.x.max, average_kind, labels.length).averages;
                        plot.setLegend({idx: plot.cursor.idx});
                    }],
                    draw: [plot => {
                        const {ctx, bbox} = plot;
                        ctx.save();
                        ctx.beginPath();
                        ctx.rect(bbox.left, bbox.top, bbox.width, bbox.height);
                        ctx.clip();
                        ctx.globalAlpha = 0.45;
                        ctx.lineWidth = uPlot.pxRatio;
                        ctx.setLineDash([6 * uPlot.pxRatio, 5 * uPlot.pxRatio]);
                        wholeAverages.forEach((average, index) => {
                            if (average == null || !plot.series[index + 1].show) return;
                            const y = plot.valToPos(average, "y", true);
                            ctx.strokeStyle = colors[index % colors.length];
                            ctx.beginPath();
                            ctx.moveTo(bbox.left, y);
                            ctx.lineTo(bbox.left + bbox.width, y);
                            ctx.stroke();
                        });
                        ctx.restore();
                    }],
                    setSelect: [plot => {
                        if (plot.select.width < 3) return;
                        setRange({
                            min: plot.posToVal(plot.select.left, "x"),
                            max: plot.posToVal(plot.select.left + plot.select.width, "x"),
                        });
                        plot.setSelect({left: 0, top: 0, width: 0, height: 0}, false);
                    }],
                },
            }, data, container);
            plots.push(plot);
            plot.over.addEventListener("dblclick", () => setRange(fullRange));
            new ResizeObserver(() => {
                const width = container.clientWidth;
                if (width > 0 && width !== plot.width) plot.setSize({width, height: 240});
            }).observe(container);
        }
        document.getElementById("reset-zoom").addEventListener("click", () => setRange(fullRange));
        lockYAxis.addEventListener("change", () => {
            for (const plot of plots) plot.setScale("y", {min: null, max: null});
        });
        status.hidden = true;
    } catch (error) {
        status.textContent = `Could not render charts: ${error.message}`;
    }
})();
