// Test implementation for time spent hover functionality
function wireTimeSpentHover() {
    var timeSpentCell = document.querySelector('.tm-cell[data-cell="time-spent"]');
    if (!timeSpentCell) return;

    console.log("Setting up hover for time spent cell");

    // Simple test: just add a tooltip that shows on hover
    timeSpentCell.addEventListener('mouseenter', function() {
        console.log("Mouse entered time spent cell");
        var tooltip = document.createElement('div');
        tooltip.textContent = 'You are selecting this task';
        tooltip.style.position = 'absolute';
        tooltip.style.backgroundColor = '#111827';
        tooltip.style.color = '#f3f4f6';
        tooltip.style.padding = '4px 8px';
        tooltip.style.borderRadius = '4px';
        tooltip.style.fontSize = '0.75rem';
        tooltip.style.zIndex = '1000';
        tooltip.style.left = '100px';
        tooltip.style.top = '100px';
        document.body.appendChild(tooltip);
    });
}