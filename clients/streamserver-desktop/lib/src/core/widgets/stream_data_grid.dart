import 'package:flutter/material.dart';
import 'package:pluto_grid/pluto_grid.dart';

import '../theme/stream_theme.dart';
import '../../utils.dart';
import '../../widgets/data_panel.dart';

typedef StreamCellRenderer = Widget Function(
  BuildContext context,
  Map<String, Object?> row,
  Object? value,
);

class StreamGridColumn {
  const StreamGridColumn({
    required this.title,
    required this.field,
    this.width = 150,
    this.minWidth = 90,
    this.renderer,
    this.enableRowChecked = false,
  });

  final String title;
  final String field;
  final double width;
  final double minWidth;
  final StreamCellRenderer? renderer;
  final bool enableRowChecked;
}

class StreamDataGrid extends StatelessWidget {
  const StreamDataGrid({
    required this.columns,
    required this.rows,
    this.height = 520,
    this.rowHeight = 48,
    this.onSelected,
    this.onDoubleTap,
    this.onSecondaryTap,
    super.key,
  });

  final List<StreamGridColumn> columns;
  final List<Map<String, Object?>> rows;
  final double height;
  final double rowHeight;
  final ValueChanged<Map<String, Object?>>? onSelected;
  final ValueChanged<Map<String, Object?>>? onDoubleTap;
  final ValueChanged<Map<String, Object?>>? onSecondaryTap;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return SizedBox(
        height: 180,
        child: Center(
          child: Text(
            '暂无数据',
            style: TextStyle(color: context.streamColors.textSecondary),
          ),
        ),
      );
    }
    final colors = context.streamColors;
    final sourcesByKey = <Key, Map<String, Object?>>{};
    Map<String, Object?> sourceFromRow(PlutoRow row) {
      final source = sourcesByKey[row.key];
      if (source != null) return source;
      final index = row.sortIdx;
      if (index >= 0 && index < rows.length) return rows[index];
      return <String, Object?>{};
    }

    final plutoColumns = columns
        .map(
          (column) => PlutoColumn(
            title: column.title,
            field: column.field,
            type: PlutoColumnType.text(),
            readOnly: true,
            width: column.width,
            minWidth: column.minWidth,
            enableRowChecked: column.enableRowChecked,
            enableColumnDrag: false,
            titlePadding:
                const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
            cellPadding:
                const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
            renderer: column.renderer == null
                ? null
                : (rendererContext) {
                    final source = sourceFromRow(rendererContext.row);
                    return Align(
                      alignment: Alignment.centerLeft,
                      child: column.renderer!(
                        context,
                        source,
                        rendererContext.cell.value,
                      ),
                    );
                  },
          ),
        )
        .toList();
    final plutoRows = <PlutoRow>[];
    for (var index = 0; index < rows.length; index++) {
      final row = rows[index];
      final key = ValueKey<Object>('stream-grid-row-$index-${row['id'] ?? ''}');
      sourcesByKey[key] = row;
      plutoRows.add(
        PlutoRow(
          key: key,
          sortIdx: index,
          cells: {
            for (final column in columns)
              column.field: PlutoCell(value: row[column.field] ?? ''),
          },
        ),
      );
    }
    return SizedBox(
      height: height,
      child: ClipRRect(
        borderRadius: BorderRadius.circular(10),
        child: PlutoGrid(
          mode: PlutoGridMode.selectWithOneTap,
          columns: plutoColumns,
          rows: plutoRows,
          onSelected: (event) {
            final row = event.row;
            if (row == null) return;
            onSelected?.call(sourceFromRow(row));
          },
          onRowDoubleTap: (event) =>
              onDoubleTap?.call(sourceFromRow(event.row)),
          onRowSecondaryTap: (event) =>
              onSecondaryTap?.call(sourceFromRow(event.row)),
          configuration: PlutoGridConfiguration(
            tabKeyAction: PlutoGridTabKeyAction.moveToNextOnEdge,
            columnSize: const PlutoGridColumnSizeConfig(
              autoSizeMode: PlutoAutoSizeMode.scale,
            ),
            style: PlutoGridStyleConfig(
              gridBackgroundColor: colors.surface,
              rowColor: colors.surface,
              evenRowColor: colors.surface,
              oddRowColor: colors.surfaceAlt.withValues(alpha: 0.35),
              activatedColor: colors.primary.withValues(alpha: 0.09),
              checkedColor: colors.primary.withValues(alpha: 0.08),
              gridBorderColor: colors.border,
              borderColor: colors.border,
              activatedBorderColor: colors.primary,
              inactivatedBorderColor: colors.border,
              iconColor: colors.textSecondary,
              menuBackgroundColor: colors.surface,
              rowHeight: rowHeight,
              columnHeight: 44,
              gridBorderRadius: BorderRadius.circular(10),
              enableColumnBorderVertical: false,
              enableCellBorderVertical: false,
              columnTextStyle: TextStyle(
                color: colors.textSecondary,
                fontSize: 12,
                fontWeight: FontWeight.w800,
              ),
              cellTextStyle: TextStyle(
                color: colors.textPrimary,
                fontSize: 13,
              ),
            ),
          ),
          noRowsWidget: const Center(child: Text('暂无数据')),
        ),
      ),
    );
  }
}

Widget gridTextCell(BuildContext context, Object? value,
    {FontWeight? fontWeight, double maxWidth = 300}) {
  return WrappedTextCell(
    value: value,
    maxWidth: maxWidth,
    fontWeight: fontWeight,
  );
}

Widget gridStatusCell(BuildContext context, Object? value) {
  return StatusBadge(status: value);
}

String gridValue(Object? value) => textValue(value);
