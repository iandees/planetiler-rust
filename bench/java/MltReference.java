import com.onthegomap.planetiler.VectorTile;
import com.onthegomap.planetiler.archive.Tile;
import com.onthegomap.planetiler.mbtiles.Mbtiles;
import com.onthegomap.planetiler.util.Gzip;
import java.io.BufferedOutputStream;
import java.io.DataOutputStream;
import java.io.FileOutputStream;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.regex.Pattern;
import java.util.stream.Collectors;
import org.maplibre.mlt.converter.ConversionConfig;
import org.maplibre.mlt.converter.FeatureTableOptimizations;
import org.maplibre.mlt.converter.MltConverter;
import org.maplibre.mlt.converter.mvt.ColumnMapping;
import org.maplibre.mlt.converter.mvt.ColumnMappingConfig;
import org.maplibre.mlt.converter.mvt.MapboxVectorTile;
import org.maplibre.mlt.data.Layer;
import org.maplibre.mlt.decoder.MltDecoder;

/**
 * Re-encodes every (gzipped MVT) tile of an mbtiles file to MLT exactly like Java Planetiler's TileArchiveWriter
 * (copy of its MLT branch and findStringColumns), so the Rust MLT encoder can be compared byte-for-byte on the same
 * input.
 *
 * <p>Usage: {@code java -cp planetiler.jar MltReference.java encode <in.mbtiles> <out.bin> [fsst] [fastpfor]
 * [tessellate] [outline] [reorder] [shareddict] [excludeids]}. Output: for each tile, big-endian ints z, x, y, length
 * followed by the MLT bytes.
 *
 * <p>{@code java -cp planetiler.jar MltReference.java decode <tiles.bin>} decodes MLT tiles written in the same format
 * with the Java MLT decoder and prints one line per feature (for comparing decoded content).
 */
public class MltReference {
  private static final Pattern MATCH_ALL = Pattern.compile(".*");
  private static final ColumnMappingConfig EMPTY_COLUMN_MAPPING = new ColumnMappingConfig();

  public static void main(String[] args) throws Exception {
    if (args[0].equals("decode")) {
      decode(Path.of(args[1]));
      return;
    }
    Set<String> opts = Set.of(args).stream().skip(0).collect(Collectors.toSet());
    boolean fsst = opts.contains("fsst");
    boolean fastpfor = opts.contains("fastpfor");
    boolean tessellate = opts.contains("tessellate");
    boolean outline = opts.contains("outline");
    boolean reorder = opts.contains("reorder");
    boolean sharedDict = opts.contains("shareddict");
    boolean includeIds = !opts.contains("excludeids");
    try (
      var db = Mbtiles.newReadOnlyDatabase(Path.of(args[1]));
      var out = new DataOutputStream(new BufferedOutputStream(new FileOutputStream(args[2])))
    ) {
      List<Tile> tiles = new ArrayList<>();
      try (var it = db.getAllTiles()) {
        while (it.hasNext()) {
          tiles.add(it.next());
        }
      }
      tiles.sort(null);
      long start = System.nanoTime();
      for (var tile : tiles) {
        var features = VectorTile.decode(Gzip.gunzip(tile.bytes()));
        Map<String, List<VectorTile.Feature>> byLayer = new LinkedHashMap<>();
        for (var f : features) {
          byLayer.computeIfAbsent(f.layer(), k -> new ArrayList<>()).add(f);
        }
        var vt = new VectorTile();
        byLayer.forEach(vt::addLayerFeatures);
        byte[] mlt = encode(vt, includeIds, fsst, fastpfor, tessellate, outline, reorder, sharedDict);
        out.writeInt(tile.coord().z());
        out.writeInt(tile.coord().x());
        out.writeInt(tile.coord().y());
        out.writeInt(mlt.length);
        out.write(mlt);
      }
      System.err.println("encoded " + tiles.size() + " tiles in " + (System.nanoTime() - start) / 1e6 + "ms");
    }
  }

  static void decode(Path path) throws Exception {
    try (var in = new java.io.DataInputStream(new java.io.BufferedInputStream(new java.io.FileInputStream(path.toFile())))) {
      while (in.available() > 0) {
        int z = in.readInt(), x = in.readInt(), y = in.readInt(), len = in.readInt();
        byte[] bytes = in.readNBytes(len);
        var tile = MltDecoder.decodeMlTile(bytes);
        for (var layer : tile.layers()) {
          for (var f : layer.features()) {
            var props = new java.util.TreeMap<String, Object>();
            f.properties().forEach((k, v) -> {
              if (v != null) {
                props.put(k, v);
              }
            });
            StringBuilder sb = new StringBuilder();
            props.forEach((k, v) -> sb.append(k).append('=').append(v).append('(').append(v.getClass().getSimpleName())
              .append(") "));
            System.out.println(z + "/" + x + "/" + y + "\t" + layer.name() + "\t" + (f.hasId() ? f.id() : "-") + "\t" +
              f.geometry().toText() + "\t" + sb.toString().trim());
          }
        }
      }
    }
  }

  static byte[] encode(VectorTile tile, boolean includeIds, boolean fsst, boolean fastpfor, boolean tessellate,
    boolean outline, boolean reorder, boolean sharedDict) throws Exception {
    MapboxVectorTile mltInput = tile.toMltInput();
    Set<String> stringColumns = new HashSet<>();
    Map<String, Set<String>> stringColumnsByLayer = new HashMap<>();
    if (sharedDict) {
      findStringColumns(mltInput, stringColumns, stringColumnsByLayer);
    }
    ColumnMappingConfig columnMappings = stringColumns.isEmpty() ? EMPTY_COLUMN_MAPPING :
      ColumnMappingConfig.of(MATCH_ALL, List.of(new ColumnMapping(stringColumns, true)));
    var tilesetMetadata = MltConverter.createTilesetMetadata(mltInput, columnMappings, includeIds, true, false);
    var conversionConfig = ConversionConfig.builder()
      .includeIds(includeIds)
      .useFastPFOR(fastpfor)
      .useFSST(fsst)
      .mismatchPolicy(ConversionConfig.TypeMismatchPolicy.COERCE)
      .optimizations(mltInput.layers().stream().collect(Collectors.toMap(
        Layer::name,
        layer -> {
          Set<String> layerStringColumns = stringColumnsByLayer.get(layer.name());
          return new FeatureTableOptimizations(reorder, !includeIds,
            layerStringColumns == null || layerStringColumns.isEmpty() ? null :
              List.of(new ColumnMapping(layerStringColumns, true)));
        }
      )))
      .preTessellatePolygons(tessellate)
      .outlineFeatureTableNames(outline ? List.of("ALL") : null)
      .useMortonEncoding(true)
      .build();
    return MltConverter.convertMvt(mltInput, tilesetMetadata, conversionConfig, null);
  }

  private static void findStringColumns(MapboxVectorTile mltInput, Set<String> stringColumns,
    Map<String, Set<String>> stringColumnsByLayer) {
    Map<String, Integer> valueCounts = new HashMap<>();
    Set<String> notStringColumns = new HashSet<>();
    int numRepeats = 0;
    boolean haveEnoughRepeatedValues = false;
    for (var layer : mltInput.layers()) {
      for (var feature : layer.features()) {
        for (var entry : feature.properties().entrySet()) {
          if (entry.getValue() instanceof String value) {
            if (!haveEnoughRepeatedValues) {
              int count = valueCounts.merge(value, 1, Integer::sum);
              if (count > 1) {
                numRepeats += count == 2 ? 2 : 1;
                if (numRepeats >= 50) {
                  haveEnoughRepeatedValues = true;
                }
              }
            }
            stringColumns.add(entry.getKey());
            stringColumnsByLayer.computeIfAbsent(layer.name(), name -> new HashSet<>()).add(entry.getKey());
          } else {
            notStringColumns.add(entry.getKey());
          }
        }
      }
    }
    if (!haveEnoughRepeatedValues) {
      stringColumns.clear();
      stringColumnsByLayer.clear();
    } else {
      stringColumns.removeAll(notStringColumns);
      stringColumnsByLayer.values().forEach(c -> c.removeAll(notStringColumns));
    }
  }
}
