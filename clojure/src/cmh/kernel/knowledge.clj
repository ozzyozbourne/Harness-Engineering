(ns cmh.kernel.knowledge
  "Lessons and verdicts: adding, recall, scoring and retirement."
  (:require [clojure.set :as set]
            [clojure.string :as str]
            [cmh.kernel.state :as st]))

(def retire-at -3)

(defn words [s]
  (set (filter #(>= (count %) 3)
               (map str/lower-case (re-seq #"[A-Za-z0-9]+" (str s))))))

(defn- norm [s] (-> (str s) str/trim (str/replace #"\s+" " ") str/lower-case))

(defn add!
  "Add a lesson (kind \"lesson\") or verdict (kind \"verdict\"). Lessons
  whose text duplicates an existing lesson are skipped; returns the new id
  or nil."
  [k kind text source]
  (let [s @(:state k)
        text (str/trim (str text))
        normed (norm text)]
    (when-not (or (str/blank? text)
                  (and (= kind "lesson")
                       (some #(= normed (norm (:text %))) (st/lessons s))))
      (let [prefix (if (= kind "verdict") "V" "L")
            n (inc (count (filter #(= kind (:kind %)) (st/entries s))))
            id (str prefix n)]
        (st/emit! k {:type "lesson_added" :id id :kind kind :text text :source source})
        (when (= kind "lesson") (st/trace k "[lesson] +" id " " text))
        id))))

(defn recall
  "Lessons (not retired) and verdicts sharing a word with query, best first."
  [s query]
  (let [qw (words query)]
    (->> (concat (st/active-lessons s) (st/verdicts s))
         (filter #(seq (set/intersection qw (words (:text %)))))
         (sort-by :score >))))

(defn score!
  "+1 to each selected lesson on success, -1 on failure; retire at <= -3."
  [k ids success?]
  (doseq [id ids]
    (let [l (get-in @(:state k) [:knowledge id])]
      (when (and l (= "lesson" (:kind l)) (not (:retired l)))
        (let [score ((if success? inc dec) (:score l))]
          (st/emit! k {:type "lesson_scored" :id id :delta (if success? 1 -1) :score score})
          (when (<= score retire-at)
            (st/emit! k {:type "lesson_retired" :id id})
            (st/trace k "[lesson] " id " retired at score " score)))))))
